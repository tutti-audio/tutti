use super::*;
use crate::{BinCount, MelBandCount, MelConfig, MelNorm};
use core::num::NonZeroUsize;
use tutti_units::Hz;

const BINS: usize = 513;
const BANDS: u16 = 26;

fn mfcc(norm: MelNorm, coefficients: u16) -> Mfcc {
    let bank = MelFilterbank::try_from(
        MelConfig::new(MelBandCount::of(BANDS), BinCount(BINS), Hz::new(15.625))
            .range(Hz::new(100.0), Hz::new(7000.0))
            .norm(norm),
    )
    .unwrap();
    Mfcc::new(bank, NonZeroUsize::new(coefficients.into()).unwrap())
}

fn spectrum(seed: u64) -> Vec<f64> {
    let mut rng = tutti_rt::SplitMix64::new(seed);
    (0..BINS)
        .map(|_| 0.1 + 2.0 * crate::testing::uniform(&mut rng, 0.0, 1.0))
        .collect()
}

/// A flat spectrum of `a` gives `N` equal log energies `ln a^2` under a `Sum` bank, so the
/// DCT leaves `c_0 = sqrt(N) ln a^2` and every other coefficient zero.
#[test]
fn a_flat_spectrum_has_only_the_zeroth_coefficient() {
    let mut m = mfcc(MelNorm::Sum, BANDS);
    let mut c = vec![9.0; BANDS as usize];
    m.process(&vec![0.7f64; BINS], &mut c);
    let want = (BANDS as f64).sqrt() * (0.7f64 * 0.7).ln();
    assert!((c[0] - want).abs() < 1e-12, "{} vs {want}", c[0]);
    for (k, v) in c.iter().enumerate().skip(1) {
        assert!(v.abs() < 1e-12, "c_{k} = {v}");
    }
}

/// Scaling the magnitudes by `g` adds `ln g^2` to each log energy: `c_0` moves by
/// `sqrt(N) ln g^2` and `c_1..` do not move.
#[test]
fn a_gain_shifts_the_zeroth_coefficient_alone() {
    let mut m = mfcc(MelNorm::Peak, 13);
    let x = spectrum(7);
    let mut base = vec![0.0; 13];
    m.process(&x, &mut base);
    for g in [0.01f64, 0.5, 3.0, 1000.0] {
        let scaled: Vec<f64> = x.iter().map(|v| v * g).collect();
        let mut c = vec![0.0; 13];
        m.process(&scaled, &mut c);
        let shift = (BANDS as f64).sqrt() * (g * g).ln();
        assert!(
            (c[0] - base[0] - shift).abs() < 1e-9 * (1.0 + shift.abs()),
            "g {g}"
        );
        for k in 1..13 {
            assert!((c[k] - base[k]).abs() < 1e-9, "g {g} c_{k}");
        }
    }
}

/// A band energy that vanishes reads the relative floor, not minus infinity: all
/// coefficients stay finite for a spectrum with one live bin.
#[test]
fn an_empty_band_reads_the_floor_not_minus_infinity() {
    let mut m = mfcc(MelNorm::Peak, 13);
    let mut x = vec![0.0f64; BINS];
    x[100] = 1.0;
    let mut c = vec![0.0; 13];
    m.process(&x, &mut c);
    assert!(c.iter().all(|v| v.is_finite()));
    assert!(c.iter().any(|&v| v != 0.0));
}

/// A silent spectrum, and one with a NaN or infinity in it, reads all zero.
#[test]
fn a_silent_or_non_finite_spectrum_reads_zero() {
    let mut m = mfcc(MelNorm::Peak, 13);
    for x in [
        vec![0.0f64; BINS],
        {
            let mut v = vec![1.0f64; BINS];
            v[50] = f64::NAN;
            v
        },
        {
            let mut v = vec![1.0f64; BINS];
            v[50] = f64::INFINITY;
            v
        },
    ] {
        let mut c = vec![5.0; 13];
        m.process(&x, &mut c);
        assert!(c.iter().all(|&v| v == 0.0), "{c:?}");
    }
}

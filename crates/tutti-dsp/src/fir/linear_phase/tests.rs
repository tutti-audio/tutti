use std::f64::consts::PI;

use tutti_units::Q;

use super::*;
use crate::{Biquad, MatchedBell, Sos, Tilt};

const RATE: SampleRate = SampleRate::SR_48K;

fn hz(v: f64) -> Hz {
    Hz::new(v)
}

fn bell(center: f64, q: f64, gain: f64) -> Sos {
    MatchedBell::new(Q::new(q), Db::new(gain))
        .design(hz(center), RATE)
        .unwrap()
}

fn tilt(center: f64, gain: f64) -> Sos {
    Tilt::new(Db::new(gain)).design(hz(center), RATE).unwrap()
}

fn taps(fir: &Fir) -> Vec<f64> {
    fir.taps().collect()
}

/// `|H(ω)|` of the taps by direct summation of `Σ h[n] e^{-jωn}`.
fn freqz(h: &[f64], omega: f64) -> f64 {
    let (mut re, mut im) = (0.0, 0.0);
    for (n, &tap) in h.iter().enumerate() {
        re += tap * (omega * n as f64).cos();
        im -= tap * (omega * n as f64).sin();
    }
    re.hypot(im)
}

/// The closed-form magnitude of a biquad: `|b0 + b1 z⁻¹ + b2 z⁻²| / |1 + a1 z⁻¹ + a2 z⁻²|`
/// on the unit circle, from `|c0 + c1 z⁻¹ + c2 z⁻²|² = c0² + c1² + c2² + 2 (c0 c1 + c1 c2)
/// cos ω + 2 c0 c2 cos 2ω`.
fn biquad_magnitude(b: &Biquad, w: f64) -> f64 {
    let power = |c0: f64, c1: f64, c2: f64| {
        c0 * c0
            + c1 * c1
            + c2 * c2
            + 2.0 * (c0 * c1 + c1 * c2) * w.cos()
            + 2.0 * c0 * c2 * (2.0 * w).cos()
    };
    (power(b.b0, b.b1, b.b2) / power(1.0, b.a1, b.a2)).sqrt()
}

/// The product of the closed-form magnitudes of every section of `bands` at `omega`.
fn target(bands: &[Sos], omega: f64) -> f64 {
    bands
        .iter()
        .flat_map(|s| s.sections())
        .map(|b| biquad_magnitude(b, omega))
        .product()
}

/// `I0(x)` and `I1(x)` by their power series.
fn bessel_i0_i1(x: f64) -> (f64, f64) {
    let (mut i0, mut i1) = (0.0, 0.0);
    let (mut term0, mut term1) = (1.0, x / 2.0);
    for k in 1..60 {
        i0 += term0;
        i1 += term1;
        let kf = f64::from(k);
        term0 *= (x / 2.0).powi(2) / (kf * kf);
        term1 *= (x / 2.0).powi(2) / (kf * (kf + 1.0));
    }
    (i0, i1)
}

/// The error the design may have against `bands` (see the module docs): the second-order
/// smoothing `½ |H''| μ₂` doubled for the higher orders, with `μ₂ = 4 β I₁(β) / (I₀(β) (N -
/// 1)²)` the window's second moment and `|H''|` the largest curvature of the target in rad²
/// (a central difference of the closed form), plus `2 · 10^(-A/20)` of its peak for the
/// window's sidelobes.
fn tolerance(bands: &[Sos], len: usize, attenuation: Db) -> f64 {
    let beta = kaiser_beta(attenuation);
    let (i0, i1) = bessel_i0_i1(beta);
    let mu2 = 4.0 * beta * i1 / (i0 * ((len - 1) * (len - 1)) as f64);
    let h = 1e-4;
    let (mut curvature, mut peak) = (0.0f64, 0.0f64);
    for i in 1..3000 {
        let w = PI * f64::from(i) / 3000.0;
        let second =
            (target(bands, w + h) - 2.0 * target(bands, w) + target(bands, w - h)) / (h * h);
        curvature = curvature.max(second.abs());
        peak = peak.max(target(bands, w));
    }
    curvature * mu2 + 2.0 * 10f64.powf(-attenuation.get() / 20.0) * peak
}

fn assert_follows_the_target(bands: &[Sos], resolution: f64, attenuation: Db) {
    let fir = LinearPhaseDesign::new(hz(resolution))
        .attenuation(attenuation)
        .design(RATE, bands)
        .unwrap();
    let h = taps(&fir);
    let tol = tolerance(bands, h.len(), attenuation);
    let mut worst = 0.0f64;
    for i in 0..=400 {
        let w = PI * f64::from(i) / 400.0;
        worst = worst.max((freqz(&h, w) - target(bands, w)).abs());
    }
    assert!(
        worst <= tol,
        "worst error {worst:e} over the tolerance {tol:e} ({} taps)",
        h.len()
    );
}

/// The taps' response is the product of the bands' closed-form magnitudes at 401 frequencies
/// from DC to Nyquist, within the window's smoothing bound.
#[test]
fn the_taps_response_is_the_product_of_the_bands_closed_form_magnitudes() {
    let bands = [
        bell(1000.0, 1.0, 9.0),
        tilt(4000.0, 6.0),
        bell(250.0, 2.0, -7.0),
    ];
    assert_follows_the_target(&bands, 40.0, Db::new(60.0));
}

/// A narrow bell, whose width is a few resolutions, is resolved within the same bound, which
/// widens with its curvature.
#[test]
fn a_narrow_bell_is_resolved_within_its_curvature_bound() {
    let bands = [bell(500.0, 8.0, 12.0)];
    assert_follows_the_target(&bands, 20.0, Db::new(80.0));
}

/// The tilt is the analog shelf `(A² x² + 1) / (x² + A²)` at `x = tan(π f / fs) / tan(π fc / fs)`.
#[test]
fn the_taps_of_a_tilt_follow_the_analog_shelf_at_the_prewarped_ratio() {
    let (fc, db) = (3000.0f64, 12.0f64);
    let fir = LinearPhaseDesign::new(hz(60.0))
        .design(RATE, &[tilt(fc, db)])
        .unwrap();
    let h = taps(&fir);
    let a2 = 10f64.powf(db / 20.0);
    let bands = [tilt(fc, db)];
    let tol = tolerance(&bands, h.len(), Db::new(60.0));
    for i in 0..=200 {
        let f = 24_000.0 * f64::from(i) / 200.0;
        let x = (PI * f / RATE.get()).tan() / (PI * fc / RATE.get()).tan();
        let want = ((a2 * x * x + 1.0) / (x * x + a2)).sqrt();
        let got = freqz(&h, 2.0 * PI * f / RATE.get());
        assert!(
            (got - want).abs() <= tol,
            "{f} Hz: {got} vs {want}, tolerance {tol:e}"
        );
    }
}

/// The length is the odd number at or above Kaiser's `(A − 7.95) / (2.285 Δω) + 1`, the taps
/// are symmetric bit for bit, and the delay is `(N − 1) / 2`.
#[test]
fn the_length_is_kaisers_estimate_made_odd_and_the_taps_are_symmetric() {
    for (resolution, atten) in [(40.0, 60.0), (10.0, 60.0), (75.0, 90.0)] {
        let fir = LinearPhaseDesign::new(hz(resolution))
            .attenuation(Db::new(atten))
            .design(RATE, &[bell(800.0, 1.5, 6.0)])
            .unwrap();
        let h = taps(&fir);
        let dw = 2.0 * PI * resolution / RATE.get();
        let estimate = ((atten - 7.95) / (2.285 * dw) + 1.0).ceil() as usize;
        assert_eq!(h.len(), estimate | 1, "resolution {resolution} Hz");
        assert_eq!(h.len() % 2, 1);
        for k in 0..h.len() {
            assert_eq!(h[k].to_bits(), h[h.len() - 1 - k].to_bits(), "tap {k}");
        }
        assert_eq!(fir.delay().get(), (h.len() - 1) / 2);
    }
}

/// No bands is the flat target of gain 1, whose zero-phase response is a unit impulse: the
/// centre tap is 1 and every other is 0, to 1e-15 (the transform's rounding).
#[test]
fn no_bands_is_a_unit_impulse_at_the_centre() {
    let none: [Sos; 0] = [];
    let fir = LinearPhaseDesign::new(hz(100.0))
        .design(RATE, &none)
        .unwrap();
    let h = taps(&fir);
    let m = (h.len() - 1) / 2;
    for (k, &tap) in h.iter().enumerate() {
        let want = if k == m { 1.0 } else { 0.0 };
        assert!((tap - want).abs() < 1e-15, "tap {k}: {tap}");
    }
}

/// A resolution or attenuation that is not positive, an attenuation past the maximum and a
/// length past the supported maximum are each refused by name.
#[test]
fn a_design_the_rate_cannot_hold_is_refused_by_name() {
    let bands = [bell(1000.0, 1.0, 3.0)];
    let design = |d: LinearPhaseDesign| d.design(RATE, &bands).unwrap_err();
    assert_eq!(
        design(LinearPhaseDesign::new(hz(0.0))),
        FirError::NonPositiveTransition(hz(0.0))
    );
    assert_eq!(
        design(LinearPhaseDesign::new(hz(10.0)).attenuation(Db::new(-3.0))),
        FirError::NonPositiveAttenuation(Db::new(-3.0))
    );
    assert!(matches!(
        design(LinearPhaseDesign::new(hz(10.0)).attenuation(Db::new(301.0))),
        FirError::AttenuationTooHigh { .. }
    ));
    assert!(matches!(
        design(LinearPhaseDesign::new(hz(0.0001)).attenuation(Db::new(100.0))),
        FirError::TooLong { max: MAX_LEN, .. }
    ));
}

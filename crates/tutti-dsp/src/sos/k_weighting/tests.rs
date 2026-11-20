use core::f64::consts::PI;

use super::*;
use crate::{Complex64, Lti};

// The literals below (here and in the prototype helpers) are BS.1770-5's printed coefficients and
// the parameters of De Man's paper (see `KWeighting`), typed again on purpose: they are the oracle
// the design's constants are checked against, not copies of them.
//

/// BS.1770-5 Annex 1, Table 1 (stage 1, the pre-filter) and Table 2 (stage 2, the RLB
/// high-pass): the 48 kHz coefficients, printed to 14 decimals.
const STAGE_1: [f64; 5] = [
    1.535_124_859_586_97,
    -2.691_696_189_406_38,
    1.198_392_810_852_85,
    -1.690_659_293_182_41,
    0.732_480_774_215_85,
];
const STAGE_2: [f64; 5] = [1.0, -2.0, 1.0, -1.990_047_454_833_98, 0.990_072_250_366_21];

fn coefficients(b: &Biquad) -> [f64; 5] {
    [b.b0, b.b1, b.b2, b.a1, b.a2]
}

/// At 48 kHz the design is the published coefficients, to their 14 printed decimals (the
/// tolerance is two orders over the print's rounding).
#[test]
fn the_48_khz_design_is_the_published_bs1770_coefficients() {
    let sos = KWeighting::design(SampleRate::SR_48K).unwrap();
    let [stage_1, stage_2] = sos.sections() else {
        panic!("two stages");
    };
    for (got, want) in coefficients(stage_1).iter().zip(STAGE_1) {
        assert!((got - want).abs() < 1e-13, "stage 1: {got} against {want}");
    }
    for (got, want) in coefficients(stage_2).iter().zip(STAGE_2) {
        assert!((got - want).abs() < 1e-13, "stage 2: {got} against {want}");
    }
}

/// `|H(j p)|` of the shelf prototype `(Vh p^2 + (Vb / Q) p + 1) / (p^2 + p / Q + 1)`.
fn shelf_magnitude(p: f64) -> f64 {
    let vh = libm::pow(10.0, 3.999_843_853_973_347 / 20.0);
    let vb = libm::pow(vh, 0.499_666_774_154_541_6);
    let q = 0.707_175_236_955_419_6;
    let j = Complex64::new(0.0, 1.0);
    let s = j * p;
    ((s * s * vh + s * (vb / q) + 1.0) / (s * s + s / q + 1.0)).norm()
}

/// `|H(j p)|` of the high-pass prototype `p^2 / (p^2 + p / Q + 1)`.
fn high_pass_magnitude(p: f64) -> f64 {
    let q = 0.500_327_037_323_877_3;
    let j = Complex64::new(0.0, 1.0);
    let s = j * p;
    ((s * s) / (s * s + s / q + 1.0)).norm()
}

/// At any rate the cascade's magnitude at `f` is the prototypes' at the prewarped
/// `p = tan(pi f / fs) / tan(pi fc / fs)` of each corner `fc`, the high-pass times its
/// `a0 = 1 + K/Q + K^2` (see [`KWeighting`]): 44.1, 48, 96 and 192 kHz agree to 1e-7 of the
/// figure (the biquad's direct evaluation near DC at 192 kHz cancels about 1e-9 of it).
#[test]
fn the_magnitude_is_the_analog_prototypes_at_the_prewarped_frequency() {
    let frequencies = [
        20.0, 38.1, 100.0, 400.0, 997.0, 1682.0, 5000.0, 10_000.0, 20_000.0,
    ];
    for rate in [44_100.0, 48_000.0, 96_000.0, 192_000.0] {
        let sos = KWeighting::design(SampleRate::new(rate).unwrap()).unwrap();
        let prewarp =
            |f: f64, corner: f64| libm::tan(PI * f / rate) / libm::tan(PI * corner / rate);
        let k = libm::tan(PI * 38.135_470_876_024_44 / rate);
        let a0 = 1.0 + k / 0.500_327_037_323_877_3 + k * k;
        for f in frequencies {
            let want = shelf_magnitude(prewarp(f, 1_681.974_450_955_533))
                * high_pass_magnitude(prewarp(f, 38.135_470_876_024_44))
                * a0;
            let mut h = [Complex64::new(0.0, 0.0)];
            sos.frequency_response(0, &[Hz::new(f)], &mut h);
            let got = h[0].norm();
            assert!(
                (got - want).abs() < 1e-7 * want,
                "{f} Hz at {rate}: {got} against {want}"
            );
        }
    }
}

/// The curve BS.1770 calibrates its `-0.691` LUFS offset to: the K-weighting reads +0.691 dB
/// at 997 Hz at 48 kHz (to the 0.005 dB the offset's three decimals carry), and 0 gain at DC
/// is the high-pass's zero.
#[test]
fn the_weighting_reads_0_691_db_at_997_hz_and_nothing_reaches_it_from_dc() {
    let sos = KWeighting::design(SampleRate::SR_48K).unwrap();
    let mut h = [Complex64::new(0.0, 0.0); 2];
    sos.frequency_response(0, &[Hz::new(997.0), Hz::new(0.0)], &mut h);
    let db = 20.0 * h[0].norm().log10();
    assert!((db - 0.691).abs() < 0.005, "{db} dB at 997 Hz");
    assert!(h[1].norm() < 1e-9, "{} at DC", h[1].norm());
}

/// A rate whose Nyquist is under the shelf's corner has no design.
#[test]
fn a_rate_under_twice_the_shelf_corner_is_refused() {
    assert!(matches!(
        KWeighting::design(SampleRate::new(3_000.0).unwrap()),
        Err(SosError::CutoffOutOfRange { .. })
    ));
}

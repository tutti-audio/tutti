use super::*;

/// `omega(x)` in `F`, read back as `f64`.
fn omega<F: Float>(x: f64) -> f64 {
    wright_omega(F::from_design(x)).to_f64()
}

/// The abscissae the identity is checked on: the whole range the float's `exp` covers on the
/// left, a fine grid across the approximation's regions, and the large positive tail.
fn grid(low: f64, high: f64) -> Vec<f64> {
    let mut xs: Vec<f64> = (0..)
        .map(|i| low + 7.0 * f64::from(i))
        .take_while(|&x| x < -30.0)
        .collect();
    xs.extend((-3000..=3000).map(|i| f64::from(i) * 0.01));
    let mut x = 30.0;
    while x <= high {
        xs.push(x);
        x *= 1.37;
    }
    xs
}

/// The defining identity `omega + ln omega = x` holds to `4 eps max(1, |x|)` (measured 1.5): the
/// rounding of the logarithm and of the sum (`eps (|x| + omega) / 2` each), and the result's
/// relative error of about `eps` times `1 + omega`. Both widths, over the left tail to where `e^x` underflows, the
/// regions of the initial approximation and the large positive tail.
fn identity_holds<F: Float>(low: f64, high: f64) {
    let eps = F::EPSILON.to_f64();
    for x in grid(low, high) {
        let w = omega::<F>(x);
        let residual = (w + w.ln() - F::from_design(x).to_f64()).abs();
        let bound = 4.0 * eps * x.abs().max(1.0);
        assert!(
            residual <= bound,
            "{x}: |w + ln w - x| = {residual:e} > {bound:e}"
        );
    }
}

#[test]
fn the_identity_w_plus_ln_w_is_x_holds_at_f64_over_minus_700_to_1e6() {
    identity_holds::<f64>(-700.0, 1.0e6);
}

#[test]
fn the_identity_w_plus_ln_w_is_x_holds_at_f32_over_minus_80_to_1e6() {
    identity_holds::<f32>(-80.0, 1.0e6);
}

/// `omega(1) = 1` exactly: the large-`x` series is exactly 1 there and the residual is 0.
#[test]
fn omega_of_one_is_one_exactly() {
    assert_eq!(wright_omega(1.0f64), 1.0);
    assert_eq!(wright_omega(1.0f32), 1.0);
}

/// `omega(0) = W(1)`, the omega constant 0.5671432904097838...; the derivative `omega / (1 +
/// omega)` is under 1, so the error is under the identity's bound `4 eps`.
#[test]
fn omega_of_zero_is_the_omega_constant() {
    const OMEGA: f64 = 0.567_143_290_409_783_8;
    assert!((wright_omega(0.0f64) - OMEGA).abs() <= 4.0 * f64::EPSILON);
    assert!((f64::from(wright_omega(0.0f32)) - OMEGA).abs() <= 4.0 * f64::from(f32::EPSILON));
}

/// `omega(x) = e^x - e^2x + 3/2 e^3x - ...` for `x` far below 0: the first terms hold to the
/// next term's size `8/3 e^4x` (bounded here by `3 e^3x`) plus the rounding of `e^x`.
#[test]
fn omega_is_e_to_the_x_minus_e_to_the_2x_far_below_zero() {
    for x in [-10.0f64, -15.0, -20.0, -30.0] {
        let t = x.exp();
        let series = t - t * t;
        let got = omega::<f64>(x);
        assert!(
            (got - series).abs() <= 3.0 * t * t * t + 4.0 * f64::EPSILON * t,
            "{x}"
        );
    }
    // `e^x` under `eps / 2` is returned as it is, and one below the float's range is 0.
    assert_eq!(wright_omega(-40.0f64), (-40.0f64).exp());
    assert_eq!(wright_omega(-1000.0f64), 0.0);
}

/// `omega(x) = L1 - L2 + L2 / L1 + L2 (L2 - 2) / (2 L1^2) + L2 (2 L2^2 - 9 L2 + 6) / (6 L1^3)
/// + O((L2 / L1)^4)` with `L1 = x`, `L2 = ln x` for large `x`: the series to its fifth term holds to
/// `(L2 / L1)^4` plus the rounding `8 eps x`.
#[test]
fn omega_is_x_minus_ln_x_and_its_series_far_above_one() {
    for x in [1.0e3f64, 1.0e4, 1.0e6, 1.0e9] {
        let l2 = x.ln();
        let series = x - l2
            + l2 / x
            + l2 * (l2 - 2.0) / (2.0 * x * x)
            + l2 * (2.0 * l2 * l2 - 9.0 * l2 + 6.0) / (6.0 * x * x * x);
        let got = omega::<f64>(x);
        let bound = (l2 / x).powi(4) + 8.0 * f64::EPSILON * x;
        assert!(
            (got - series).abs() <= bound,
            "{x}: {} vs {bound:e}",
            got - series
        );
    }
}

#[test]
fn the_ends_and_nan() {
    assert!(wright_omega(f64::NAN).is_nan());
    assert!(wright_omega(f32::NAN).is_nan());
    assert_eq!(wright_omega(f64::NEG_INFINITY), 0.0);
    assert_eq!(wright_omega(f64::INFINITY), f64::INFINITY);
    assert_eq!(wright_omega(f32::INFINITY), f32::INFINITY);
}

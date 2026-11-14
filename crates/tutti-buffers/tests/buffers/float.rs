//! `Float` is zero, one and the analytic values of its math.

use tutti_buffers::Float;

fn laws<F: Float>(tol: F) {
    let near = |a: F, b: F| (a - b).abs() <= tol;
    assert_eq!(F::ZERO + F::ONE, F::ONE);
    let two = F::ONE + F::ONE;
    assert_eq!((two * two).sqrt(), two);
    assert!(near(F::ZERO.exp(), F::ONE));
    assert!(near(F::ONE.ln(), F::ZERO));
    assert!(near(two.powf(two), two * two));
    assert!(near(F::ZERO.sin(), F::ZERO));
    assert!(near(F::ZERO.cos(), F::ONE));
    assert_eq!((-two).abs(), two);
    let half = F::ONE / two;
    assert_eq!((F::ONE + half).floor(), F::ONE);
    assert_eq!((F::ONE + half).ceil(), two);
    assert_eq!((-half).floor(), -F::ONE);
    assert!(near(F::ZERO.tan(), F::ZERO));
    assert!(near(F::ZERO.tanh(), F::ZERO));
    assert_eq!(two.min(half), half);
    assert_eq!(two.max(half), two);
}

/// tan(π/4) = 1, tanh saturates at ±1, and min/max return the number when the
/// other side is NaN (IEEE 754 minNum/maxNum).
#[test]
fn tan_tanh_min_max_meet_their_analytic_values() {
    assert!((<f64 as Float>::tan(core::f64::consts::FRAC_PI_4) - 1.0).abs() < 1e-15);
    assert!((<f32 as Float>::tan(core::f32::consts::FRAC_PI_4) - 1.0).abs() < 1e-6);
    assert_eq!(<f64 as Float>::tanh(40.0), 1.0);
    assert_eq!(<f32 as Float>::tanh(-40.0), -1.0);
    assert_eq!(<f32 as Float>::min(f32::NAN, 0.5), 0.5);
    assert_eq!(<f64 as Float>::max(-0.5, f64::NAN), -0.5);
}

#[test]
fn f32_math_matches_its_analytic_values() {
    laws::<f32>(1e-6);
}

#[test]
fn f64_math_matches_its_analytic_values() {
    laws::<f64>(1e-12);
}

#[test]
fn exp_of_one_is_euler_s_number() {
    assert!((<f64 as Float>::exp(1.0) - core::f64::consts::E).abs() < 1e-15);
    assert!((<f32 as Float>::exp(1.0) - core::f32::consts::E).abs() < 1e-6);
}

/// The constants are their analytic values, bit for bit where the standard
/// library has the same constant.
#[test]
fn the_constants_are_the_analytic_values() {
    assert_eq!(<f32 as Float>::PI, core::f32::consts::PI);
    assert_eq!(<f64 as Float>::PI, core::f64::consts::PI);
    assert_eq!(<f32 as Float>::TAU, 2.0 * core::f32::consts::PI);
    assert_eq!(<f64 as Float>::TAU, 2.0 * core::f64::consts::PI);
    assert_eq!(<f32 as Float>::EPSILON.to_bits(), 0x3400_0000, "2^-23");
    assert_eq!(<f64 as Float>::EPSILON, 2.0f64.powi(-52));
    assert_eq!(<f32 as Float>::INFINITY, f32::INFINITY);
    assert_eq!(<f64 as Float>::INFINITY, f64::INFINITY);
    assert!(<f32 as Float>::NAN.is_nan() && <f64 as Float>::NAN.is_nan());
    assert_eq!(<f32 as Float>::ZERO.to_bits(), 0);
    assert_eq!(<f64 as Float>::ONE, 1.0);
}

/// −3 dB is `1/√2`: the bits of the standard constant, a half when squared
/// (−3.0103 dB), and 0.7071.
#[test]
#[allow(
    clippy::approx_constant,
    reason = "the figure is pinned independently of the constant under test"
)]
fn minus_3_db_is_one_over_root_two() {
    assert_eq!(
        <f32 as Float>::MINUS_3_DB.to_bits(),
        core::f32::consts::FRAC_1_SQRT_2.to_bits()
    );
    assert_eq!(
        <f64 as Float>::MINUS_3_DB.to_bits(),
        core::f64::consts::FRAC_1_SQRT_2.to_bits()
    );
    assert!((<f32 as Float>::MINUS_3_DB - 0.7071).abs() < 1e-4);
    assert!((<f64 as Float>::MINUS_3_DB.powi(2) - 0.5).abs() < 1e-15);
    let db = 20.0 * <f64 as Float>::log10(<f64 as Float>::MINUS_3_DB);
    assert!((db + 3.0103).abs() < 1e-4, "{db}");
}

fn classify<F: Float>() {
    let two = F::ONE + F::ONE;
    assert!(F::NAN.is_nan() && !F::ONE.is_nan() && !F::INFINITY.is_nan());
    assert!(F::ONE.is_finite() && F::ZERO.is_finite());
    assert!(!F::INFINITY.is_finite() && !(-F::INFINITY).is_finite() && !F::NAN.is_finite());
    assert_eq!(two.clamp(F::ZERO, F::ONE), F::ONE, "above");
    assert_eq!((-two).clamp(F::ZERO, F::ONE), F::ZERO, "below");
    assert_eq!(
        (F::ONE / two).clamp(F::ZERO, F::ONE),
        F::ONE / two,
        "inside"
    );
    assert!(F::NAN.clamp(F::ZERO, F::ONE).is_nan(), "a NaN is unchanged");
    assert_eq!(two.signum(), F::ONE);
    assert_eq!((-two).signum(), -F::ONE);
    assert_eq!(F::ZERO.signum(), F::ONE);
    assert_eq!((-F::ZERO).signum(), -F::ONE, "negative zero has a sign");
    assert!(F::NAN.signum().is_nan());
}

#[test]
fn is_nan_is_finite_clamp_and_signum_classify() {
    classify::<f32>();
    classify::<f64>();
}

/// One rounding: `(1 + 2^-k)² - (1 + 2^-(k-1))` is `2^-2k` exactly when the
/// product is not rounded first, and zero when it is.
#[test]
fn mul_add_rounds_once() {
    let a = 1.0f64 + 2.0f64.powi(-30);
    let c = -(1.0 + 2.0f64.powi(-29));
    assert_eq!(a * a + c, 0.0, "the unfused form loses the low term");
    assert_eq!(<f64 as Float>::mul_add(a, a, c), 2.0f64.powi(-60));
    let a = 1.0f32 + 2.0f32.powi(-13);
    let c = -(1.0 + 2.0f32.powi(-12));
    assert_eq!(a * a + c, 0.0);
    assert_eq!(<f32 as Float>::mul_add(a, a, c), 2.0f32.powi(-26));
    assert_eq!(<f32 as Float>::mul_add(2.0, 3.0, 4.0), 10.0);
}

fn transcendentals<F: Float>(tol: F) {
    let near = |a: F, b: F| (a - b).abs() <= tol;
    let two = F::ONE + F::ONE;
    let ten = F::from_f32(10.0);
    assert!(near(F::from_f32(1000.0).log10(), F::from_f32(3.0)));
    assert!(near(ten.exp2(), F::from_f32(1024.0)));
    assert!(near(F::ONE.atan2(F::ONE), F::PI / F::from_f32(4.0)));
    assert!(near(F::ONE.atan2(-F::ONE), F::PI * F::from_f32(0.75)));
    assert!(near((-F::ONE).atan2(F::ZERO), -F::PI / two));
    assert!(
        near(F::ZERO.atan2(-F::ONE), F::PI),
        "the negative real axis is π"
    );
    assert!(near(F::ONE.exp2().log10(), two.log10()));
}

#[test]
fn log10_exp2_and_atan2_meet_their_analytic_values() {
    transcendentals::<f32>(1e-6);
    transcendentals::<f64>(1e-12);
}

#[test]
fn from_f32_and_to_f32_cross_the_two_floats() {
    let x = 0.1f32;
    assert_eq!(
        <f64 as Float>::from_f32(x),
        f64::from(x),
        "widening is exact"
    );
    assert_eq!(<f32 as Float>::from_f32(x).to_bits(), x.to_bits());
    assert_eq!(<f32 as Float>::to_f32(x).to_bits(), x.to_bits());
    let fine = 1.0f64 + 2.0f64.powi(-30);
    assert_eq!(<f64 as Float>::to_f32(fine), 1.0, "narrowing rounds");
    assert_eq!(<f64 as Float>::to_f32(f64::from(x)).to_bits(), x.to_bits());
}

/// A generic function reaches everything `Float` promises without naming a
/// float: display, sums, remainders and the compound assignments.
fn generic<F: Float>() -> (F, F, String) {
    let xs = [F::ONE, F::from_f32(2.0), F::from_f32(3.0)];
    let total: F = xs.iter().copied().sum();
    let mut x = total % F::from_f32(4.0);
    x /= F::from_f32(2.0);
    x *= F::from_f32(3.0);
    x -= F::ONE;
    x += F::from_f32(0.5);
    (total, x, format!("{}", xs[1]))
}

#[test]
fn float_bounds_sum_rem_div_assign_and_display() {
    assert_eq!(generic::<f32>(), (6.0, 2.5, "2".to_string()));
    assert_eq!(generic::<f64>(), (6.0, 2.5, "2".to_string()));
}

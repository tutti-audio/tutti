use super::*;

/// `f(u) = u^2 - 2` from 3/2 takes 17/12 then 577/408 (`u <- (u + 2 / u) / 2`, exact in
/// rationals): exactly [`NEWTON_CAP`] steps, no early exit and no tolerance test.
#[test]
fn two_newton_steps_of_the_square_root_of_two_from_three_halves_are_577_over_408() {
    let root = capped_newton(|u: f64| (u * u - 2.0, 2.0 * u), 1.5, 1.5);
    assert!((root - 577.0 / 408.0).abs() <= 2.0 * f64::EPSILON, "{root}");
    assert_eq!(NEWTON_CAP, 2);
}

/// A solution that is already exact stays put: the steps are taken and change nothing.
#[test]
fn an_exact_start_is_a_fixed_point() {
    let root = capped_newton(|u: f64| (u - 3.0, 1.0), 3.0, 0.0);
    assert_eq!(root, 3.0);
}

/// The error `g(g(e0))` two Newton steps on `e^u - c` leave from `e0` below the root, with
/// 1e-12 for the rounding of the exponential at 1000.
fn after_two_steps(e0: f64) -> f64 {
    let g = |e: f64| e.exp() - 1.0 - e;
    g(g(e0)) + 1e-12
}

/// `f(u) = e^u - 1000`: a Newton step from `e0` below the root `ln 1000` leaves exactly
/// `e^e0 - 1 - e0` (`u <- u + 1000 e^-u - 1`), so two steps leave `g(g(e0))` for
/// `g(e) = e^e - 1 - e`, 4.6e-10 from `e0 = 0.0078`. The seed 0 is 6.9 away (a step from it
/// lands near 999); the warm start 6.9 is the closer. The warm start wins, and the seed wins in
/// the mirrored call.
#[test]
fn the_start_with_the_smaller_residual_is_taken() {
    let f = |u: f64| (u.exp() - 1000.0, u.exp());
    let root = 1000.0f64.ln();
    let bound = after_two_steps(root - 6.9);
    assert!((capped_newton(f, 0.0, 6.9) - root).abs() <= bound);
    assert!((capped_newton(f, 6.9, 0.0) - root).abs() <= bound);
}

/// A warm start that is NaN (a state a non-finite input left) loses to the seed.
#[test]
fn a_nan_warm_start_takes_the_seed() {
    let f = |u: f64| (u.exp() - 1000.0, u.exp());
    let root = 1000.0f64.ln();
    assert!((capped_newton(f, 6.9, f64::NAN) - root).abs() <= after_two_steps(root - 6.9));
}

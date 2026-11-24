use super::*;

// The tests other than the `f64` ones pin the `f32` instantiation, the session's audio
// format, so their tolerances are `f32`'s.

/// In `f64`, `n` steps of coefficient `c` from `y0` toward a wide target `t` land on the
/// closed form `t - (t - y0) * (1 - c)^n` to `2^-45`, a bound an `f32` state cannot meet.
#[test]
fn an_f64_smoother_follows_the_closed_form_of_the_one_pole() {
    use tutti_fixtures::signal::{assert_close, wide_signal};
    let (y0, t) = (wide_signal(1)[0], wide_signal(40)[39]);
    let c = 0.125_f64;
    let mut y = y0;
    let got: Vec<f64> = (0..32)
        .map(|_| {
            y = smooth_toward(y, t, c);
            y
        })
        .collect();
    let want: Vec<f64> = (1..=32).map(|n| t - (t - y0) * (1.0 - c).powi(n)).collect();
    assert_close(&got, &want, 2f64.powi(-45));
}

/// The `f32` step is the same expression: a tenth of the way up from 0 is `0.1_f32`'s product,
/// the full gap at 1 is the target, and none at 0 holds.
#[test]
fn an_f32_step_closes_the_coefficient_of_the_gap() {
    assert_eq!(smooth_toward(0.0_f32, 1.0, 0.1), 0.1_f32);
    assert_eq!(smooth_toward(1.0_f32, 0.0, 0.1), 0.9_f32);
    assert_eq!(smooth_toward(1.0_f32, 0.0, 1.0), 0.0);
    assert_eq!(smooth_toward(0.25_f32, 1.0, 0.0), 0.25);
}

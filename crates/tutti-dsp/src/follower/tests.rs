use tutti_units::{Amplitude, Ballistics, SampleRate, Seconds};

use super::Follower;
use crate::testing::{assert_close_f64, wide_signal};

const RATE: f64 = 48_000.0;

fn follower<F: tutti_buffers::Float>() -> (Follower<F>, f32, f32) {
    let ballistics = Ballistics::new(Seconds(0.01), Seconds(0.1)).expect("a valid pair");
    let (attack, release) = ballistics.coeffs(SampleRate::of(RATE));
    (
        Follower::of(ballistics, SampleRate::of(RATE)),
        attack,
        release,
    )
}

/// From 0 toward a target, one step is `attack * target`: in `f64` the 29-bit target
/// scaled by the `f32` attack coefficient widened exactly, with no narrowing of the state.
#[test]
fn an_f64_attack_step_from_zero_is_the_coefficient_times_the_target() {
    let (follower, attack, _) = follower::<f64>();
    let targets = wide_signal(64);
    let got: Vec<f64> = targets
        .iter()
        .map(|&t| follower.follow(Seconds(0.0), Seconds(t)).get())
        .collect();
    let want: Vec<f64> = targets.iter().map(|&t| f64::from(attack) * t).collect();
    assert_close_f64(&got, &want, 0.0);
}

/// From a target down to 0, one step keeps `1 - release` of the state: `t * (1 - release)`
/// to the rounding of one `f64` subtraction, far below the `f32` ulp.
#[test]
fn an_f64_release_step_to_zero_keeps_one_minus_the_coefficient() {
    let (follower, _, release) = follower::<f64>();
    let states = wide_signal(64);
    let got: Vec<f64> = states
        .iter()
        .map(|&s| follower.follow(Seconds(s), Seconds(0.0)).get())
        .collect();
    let want: Vec<f64> = states
        .iter()
        .map(|&s| s * (1.0 - f64::from(release)))
        .collect();
    assert_close_f64(&got, &want, 2f64.powi(-50));
}

/// The neutral follower closes none of the gap, so a wide state stays exactly where it is.
#[test]
fn an_f64_neutral_follower_holds_a_wide_state_exactly() {
    let held = wide_signal(16);
    let got: Vec<f64> = held
        .iter()
        .map(|&s| {
            Follower::<f64>::NEUTRAL
                .follow(Seconds(s), Seconds(0.0))
                .get()
        })
        .collect();
    assert_close_f64(&got, &held, 0.0);
}

/// The same steps in `f32` are `attack * target` and `state * (1 - release)` to `f32` rounding.
#[test]
fn the_f32_follower_takes_the_same_one_step_values() {
    let (follower, attack, release) = follower::<f32>();
    let up = follower.follow(Amplitude(0.0), Amplitude(0.5)).get();
    assert_eq!(up, attack * 0.5);
    let down = follower.follow(Amplitude(0.5), Amplitude(0.0)).get();
    assert!((down - 0.5 * (1.0 - release)).abs() < 1e-7, "{down}");
}

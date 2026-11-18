use tutti_units::{Amplitude, Ballistics, SampleRate, Seconds};

use super::Follower;
use tutti_fixtures::signal::{assert_close_f64, wide_signal};

// The tests other than the `f64` ones pin the `f32` instantiation, the session's audio
// format, so their tolerances are `f32`'s.

const RATE: f64 = 48_000.0;

const ATTACK: f64 = 0.01;
const RELEASE: f64 = 0.1;

/// The one-pole coefficient `1 - exp(-1 / (tau * rate))` in `f64`.
fn closed(tau: f64) -> f64 {
    1.0 - (-1.0 / (tau * RATE)).exp()
}

fn follower<F: tutti_buffers::Float>() -> Follower<F> {
    let ballistics = Ballistics::new(Seconds(ATTACK), Seconds(RELEASE)).expect("a valid pair");
    Follower::of(ballistics, SampleRate::of(RATE))
}

/// From 0 toward a target, one step is `attack * target` with the closed-form `f64` attack
/// coefficient, to `1e-12` relative: an `f32` coefficient is off by about `1e-8`.
#[test]
fn an_f64_attack_step_from_zero_is_the_coefficient_times_the_target() {
    let follower = follower::<f64>();
    let targets = wide_signal(64);
    let got: Vec<f64> = targets
        .iter()
        .map(|&t| follower.follow(Seconds(0.0), Seconds(t)).get())
        .collect();
    let want: Vec<f64> = targets.iter().map(|&t| closed(ATTACK) * t).collect();
    assert_close_f64(&got, &want, 1e-12);
}

/// From a target down to 0, one step keeps `1 - release` of the state, with the closed-form
/// `f64` coefficient, to `1e-12`.
#[test]
fn an_f64_release_step_to_zero_keeps_one_minus_the_coefficient() {
    let follower = follower::<f64>();
    let states = wide_signal(64);
    let got: Vec<f64> = states
        .iter()
        .map(|&s| follower.follow(Seconds(s), Seconds(0.0)).get())
        .collect();
    let want: Vec<f64> = states
        .iter()
        .map(|&s| s * (1.0 - closed(RELEASE)))
        .collect();
    assert_close_f64(&got, &want, 1e-12);
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

/// The same steps in `f32` are `attack * target` exactly and `state * (1 - release)` to `f32`
/// rounding (`1 - release` rounds before the product).
#[test]
fn the_f32_follower_takes_the_same_one_step_values() {
    let follower = follower::<f32>();
    let (attack, release) = (closed(ATTACK) as f32, closed(RELEASE) as f32);
    let up = follower.follow(Amplitude(0.0), Amplitude(0.5)).get();
    assert_eq!(up, attack * 0.5);
    let down = follower.follow(Amplitude(0.5), Amplitude(0.0)).get();
    assert!(
        (down - 0.5 * (1.0 - release)).abs() <= 0.5 * 2f32.powi(-24),
        "{down}"
    );
}

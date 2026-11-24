use tutti_units::{Ballistics, SampleRate, Seconds};

use super::{Follower, Pole, Smoothed};
use tutti_fixtures::signal::{assert_close, wide_signal};

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
    let ballistics =
        Ballistics::new(Seconds::new(ATTACK), Seconds::new(RELEASE)).expect("a valid pair");
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
        .map(|&t| follower.follow_raw(&mut Smoothed::at(0.0), t))
        .collect();
    let want: Vec<f64> = targets.iter().map(|&t| closed(ATTACK) * t).collect();
    assert_close(&got, &want, 1e-12);
}

/// From a target down to 0, one step keeps `1 - release` of the state, with the closed-form
/// `f64` coefficient, to `1e-12`.
#[test]
fn an_f64_release_step_to_zero_keeps_one_minus_the_coefficient() {
    let follower = follower::<f64>();
    let states = wide_signal(64);
    let got: Vec<f64> = states
        .iter()
        .map(|&s| follower.follow_raw(&mut Smoothed::at(s), 0.0))
        .collect();
    let want: Vec<f64> = states
        .iter()
        .map(|&s| s * (1.0 - closed(RELEASE)))
        .collect();
    assert_close(&got, &want, 1e-12);
}

/// The neutral follower closes none of the gap, so a wide state stays exactly where it is.
#[test]
fn an_f64_neutral_follower_holds_a_wide_state_exactly() {
    let held = wide_signal(16);
    let got: Vec<f64> = held
        .iter()
        .map(|&s| Follower::<f64>::NEUTRAL.follow_raw(&mut Smoothed::at(s), 0.0))
        .collect();
    assert_close(&got, &held, 0.0);
}

/// The same steps in `f32` are `attack * target` exactly and `state * (1 - release)` to `f32`
/// rounding (`1 - release` rounds before the product).
#[test]
fn the_f32_follower_takes_the_same_one_step_values() {
    let follower = follower::<f32>();
    let (attack, release) = (closed(ATTACK) as f32, closed(RELEASE) as f32);
    let up = follower.follow_raw(&mut Smoothed::at(0.0), 0.5);
    assert_eq!(up, attack * 0.5);
    let down = follower.follow_raw(&mut Smoothed::at(0.5), 0.0);
    assert!(
        (down - 0.5 * (1.0 - release)).abs() <= 0.5 * 2f32.powi(-24),
        "{down}"
    );
}

/// A time constant of up to 1.44 frames (a coefficient over one half) is a pole that leaves
/// `exp(-1 / n)` of the gap, and a longer one a pole that closes `1 - exp(-1 / n)`; each is a step
/// of the same one-pole (`f64`, to rounding).
#[test]
fn a_pole_leaves_its_exponential_to_one_half_and_closes_its_complement_beyond() {
    let rate = SampleRate::of(RATE);
    let frames = |n: f64| Seconds::new(n / RATE);
    assert_eq!(
        Pole::<f64>::of(frames(1.0), rate),
        Pole::Leaves((-1.0f64).exp())
    );
    assert_eq!(
        Pole::<f64>::of(frames(3.0), rate),
        Pole::Closes(-(-1.0f64 / 3.0).exp_m1())
    );
    assert_eq!(Pole::<f64>::of(Seconds::ZERO, rate), Pole::<f64>::INSTANT);
    assert_eq!(
        Pole::<f64>::of(Seconds::new(f64::INFINITY), rate),
        Pole::<f64>::HOLD
    );
    for n in [0.5, 1.0, 1.4, 1.5, 3.0, 100.0] {
        let mut state = Smoothed::at(0.3);
        let got = Pole::<f64>::of(frames(n), rate).step(&mut state, 1.1);
        let want = 1.1 + (0.3 - 1.1) * (-1.0 / n).exp();
        assert!((got - want).abs() < 1e-15, "{n} frames: {got} vs {want}");
    }
}

/// The f32 range extreme of the leave form: a time constant of a fraction of a frame at 192 kHz
/// (a step that leaves `r = exp(-1 / n)` of the gap, `n` frames), over 200 time constants from
/// 0.05 to 0.7 frames, so `r` runs from 2e-9 to 0.24. Four steps to 0 from 1 leave `r^4`. Each
/// step is one product: `r` is the `f64` design rounded once (a relative error of at most
/// `2^-24`) and the product rounds once more, so the state is `r^4` to `4 * 2 * 2^-24` relative,
/// whatever `r` is: a deterministic bound with no noise term. The coefficient `c = 1 - r`
/// rounded to f32 and applied as `s + c (0 - s)` has an absolute error of up to `2^-25` in
/// `1 - c`, so the relative error in `r` is `2^-25 / r`, which is over 100 times the bound at
/// the worst point of the sweep.
#[test]
fn an_f32_sub_frame_time_constant_keeps_its_retained_fraction_where_the_coefficient_form_loses_it()
{
    let rate = SampleRate::SR_192K;
    let (mut stored, mut coefficient) = (0.0f64, 0.0f64);
    for i in 0..200 {
        let frames = 0.05 + 0.65 * f64::from(i) / 199.0;
        let tau = Seconds::new(frames / rate.get());
        let keep = (-1.0 / frames).exp();
        let pole = Pole::<f32>::of(tau, rate);
        let c = tau.one_pole_coeff(rate) as f32;
        let (mut new, mut old) = (Smoothed::at(1.0f32), 1.0f32);
        for _ in 0..4 {
            pole.step(&mut new, 0.0);
            old += c * (0.0 - old);
        }
        let want = keep.powi(4);
        stored = stored.max(((f64::from(new.value()) - want) / want).abs());
        coefficient = coefficient.max(((f64::from(old) - want) / want).abs());
    }
    assert!(stored <= 8.0 * 2f64.powi(-24), "leave form {stored:e}");
    assert!(
        coefficient > 100.0 * 8.0 * 2f64.powi(-24),
        "the coefficient form {coefficient:e} is not clearly worse than {stored:e}"
    );
}

/// An f32 one-pole of 100 s and of 300 s at 192 kHz (coefficients `5.2e-8` and `1.7e-8`, under
/// `2^-24`) reaches `1 - 1/e` of a unit step after `tau * rate` frames to within one frame's
/// advance, `c / e`, plus the value's own rounding, `2^-24`, because the part of each step the
/// value's ulp drops is carried into the next. The same recurrence with that part dropped,
/// `s + c (1 - s)` in f32, adds less than half an ulp of the value from `s = 1/2` on and stalls
/// there, `0.13` short.
#[test]
fn a_compensated_f32_follower_reaches_one_minus_one_over_e_in_tau_frames_where_the_plain_recurrence_stalls()
 {
    let rate = SampleRate::SR_192K;
    let want = 1.0 - (-1.0f64).exp();
    for tau in [100.0, 300.0] {
        let n = (tau * rate.get()) as u64;
        let pole = Pole::<f32>::of(Seconds::new(tau), rate);
        let Pole::Closes(c) = pole else {
            panic!("{pole:?}")
        };
        assert!(f64::from(c) < 2f64.powi(-24), "{tau} s: c {c}");
        let (mut state, mut plain) = (Smoothed::ZERO, 0.0f32);
        let mut frozen_at = None;
        for k in 0..n {
            pole.step(&mut state, 1.0);
            let next = plain + c * (1.0 - plain);
            if next == plain && frozen_at.is_none() {
                frozen_at = Some(k);
            }
            plain = next;
        }
        let bound = f64::from(c) * (-1.0f64).exp() + 2f64.powi(-24);
        let got = f64::from(state.value());
        assert!(
            (got - want).abs() <= bound,
            "{tau} s: {got} vs {want}, bound {bound:e}"
        );
        let stalled = f64::from(plain);
        assert!(
            frozen_at.is_some(),
            "{tau} s: the plain recurrence never froze"
        );
        assert!(
            (stalled - want).abs() > 0.1 && stalled <= 0.5 + 1e-6,
            "{tau} s: the plain recurrence reads {stalled}, not stalled at one half"
        );
    }
}

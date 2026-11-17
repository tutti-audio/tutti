use super::*;

const RATE: f64 = 48_000.0;

fn pair(attack: f64, release: f64) -> Ballistics {
    Ballistics::new(Seconds(attack), Seconds(release)).expect("a valid pair")
}

/// The coefficients are `1 - exp(-1 / (t * rate))`, the one-pole step that closes `1 - 1/e`
/// of the gap in `t` seconds, pinned against the closed form.
#[test]
fn coeffs_are_one_minus_exp_of_minus_one_over_time_times_rate() {
    let (attack, release) = pair(0.01, 0.1).coeffs(SampleRate::of(RATE));
    let closed = |t: f64| (1.0 - (-1.0 / (t * RATE)).exp()) as f32;
    assert!((attack - closed(0.01)).abs() < 1e-9, "{attack}");
    assert!((release - closed(0.1)).abs() < 1e-9, "{release}");
}

/// A step of one sample of audio time is the per-sample coefficient.
#[test]
fn coeff_over_one_sample_is_the_coefficient_at_that_rate() {
    let b = pair(0.01, 0.1);
    let (attack, release) = b.coeffs(SampleRate::of(RATE));
    let dt = Seconds(1.0 / RATE);
    assert!((b.coeff_over(true, dt) - attack).abs() < 1e-9);
    assert!((b.coeff_over(false, dt) - release).abs() < 1e-9);
}

/// Over a step of one time constant a one-pole closes `1 - 1/e` of the gap, the attack's for
/// a rising quantity and the release's for a falling one.
#[test]
fn coeff_over_one_time_constant_closes_one_minus_inverse_e() {
    let b = pair(0.01, 0.1);
    let want = 1.0 - (-1.0f64).exp();
    assert!((f64::from(b.coeff_over(true, Seconds(0.01))) - want).abs() < 1e-6);
    assert!((f64::from(b.coeff_over(false, Seconds(0.1))) - want).abs() < 1e-6);
}

/// The convention: an instant (zero) time is coefficient 1, closing the whole gap, and a
/// positive time over a step that is not above zero holds: 0.
#[test]
fn a_zero_time_is_coefficient_one_and_a_zero_step_holds() {
    let b = pair(0.0, 0.0);
    assert_eq!(b.coeffs(SampleRate::of(RATE)), (1.0, 1.0));
    assert_eq!(b.coeff_over(true, Seconds(0.001)), 1.0);
    assert_eq!(b.coeff_over(false, Seconds(0.001)), 1.0);
    assert_eq!(pair(0.1, 0.1).coeff_over(true, Seconds(0.0)), 0.0);
}

/// A time that is negative, NaN or infinite is refused, in either position.
#[test]
fn a_negative_or_non_finite_time_is_refused() {
    for bad in [-0.001, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(Ballistics::new(Seconds(bad), Seconds(0.1)), None, "{bad}");
        assert_eq!(Ballistics::new(Seconds(0.1), Seconds(bad)), None, "{bad}");
    }
    let b = pair(0.01, 0.1);
    assert_eq!((b.attack(), b.release()), (Seconds(0.01), Seconds(0.1)));
}

/// A document holds only a valid pair: a refused time fails to deserialize.
#[cfg(feature = "serde")]
#[test]
fn serde_round_trips_a_pair_and_refuses_a_negative_time() {
    let b = pair(0.01, 0.1);
    let json = serde_json::to_string(&b).unwrap();
    assert_eq!(serde_json::from_str::<Ballistics>(&json).unwrap(), b);
    assert!(serde_json::from_str::<Ballistics>("[-1.0, 0.1]").is_err());
    assert!(serde_json::from_str::<Ballistics>("[0.1, 1e999]").is_err());
}

use super::*;

/// A pole of 0.5 halves each frame, so 2^-10 is reached after 10 frames; 6.020599913 dB
/// per halving puts 2^-10 at -20 log10(1024) dB.
#[test]
fn ring_frames_of_a_half_pole_to_two_to_the_minus_ten_is_ten() {
    let floor = Db::from_amplitude(tutti_units::Amplitude(1.0 / 1024.0));
    assert!((ring_frames(0.5, floor) - 10.0).abs() < 1e-12);
}

#[test]
fn ring_frames_edges_are_instant_for_pole_zero_and_never_for_pole_one() {
    assert_eq!(ring_frames(0.0, Db(-60.0)), 0.0);
    assert_eq!(ring_frames(1.0, Db(-60.0)), f64::INFINITY);
}

/// `exp(-2 pi fc / fs)` for fc = fs / (2 pi) is `exp(-1)`.
#[test]
fn cutoff_pole_at_rate_over_two_pi_is_exp_minus_one() {
    let rate = SampleRate::SR_48K;
    let fc = Hz(48_000.0 / std::f64::consts::TAU);
    assert!((cutoff_pole(fc, rate) - (-1.0f64).exp()).abs() <= f64::EPSILON);
}

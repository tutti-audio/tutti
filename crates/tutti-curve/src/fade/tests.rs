//! The fade laws against their closed forms. The spec is the oracle: the
//! figures are the smootherstep and the sine/cosine pair written out here.

use super::*;
use crate::Piece;

const LENGTHS: [usize; 8] = [1, 2, 3, 7, 64, 240, 1000, 4800];

fn smootherstep(x: f64) -> f64 {
    x * x * x * (6.0 * x * x - 15.0 * x + 10.0)
}

/// `EqualAmplitude`'s incoming gain is the smootherstep of `(k + 1) / (len +
/// 1)`, computed here in `f64`, at every frame of fades of many lengths, and
/// its outgoing gain the complement. Both gains stay in `[0, 1]`.
#[test]
fn equal_amplitude_is_the_smootherstep_at_every_frame() {
    for len in LENGTHS {
        for k in 0..len {
            let (out, inn) = FadeLaw::EqualAmplitude.gains(k, len);
            assert!(
                (0.0..=1.0).contains(&out) && (0.0..=1.0).contains(&inn),
                "{k}/{len}: ({out}, {inn})"
            );
            let want = smootherstep((k + 1) as f64 / (len + 1) as f64);
            assert!((f64::from(inn) - want).abs() < 5e-6, "{k}/{len}: {inn}");
            assert!(
                (f64::from(out) - (1.0 - want)).abs() < 5e-6,
                "{k}/{len}: {out}"
            );
        }
    }
}

/// At a position that is a dyadic fraction every step of the polynomial is
/// exact in `f32`: `gains(0, 3)` is `x = 1/4`, `x³(6x² − 15x + 10) =
/// 0.103515625`.
#[test]
fn equal_amplitude_is_the_smootherstep_exactly_at_dyadic_positions() {
    for (k, len, want) in [
        (0usize, 3usize, 0.103_515_625f64),
        (1, 3, 0.5),
        (2, 3, 0.896_484_375),
        (0, 7, 0.016_052_246_093_75),
        (5, 7, 0.896_484_375),
    ] {
        let (out, inn) = FadeLaw::EqualAmplitude.gains(k, len);
        assert_eq!(f64::from(inn), want, "{k}/{len}");
        assert_eq!(f64::from(out), 1.0 - want, "{k}/{len}: the complement");
    }
}

/// `EqualAmplitude` sums to one at every frame, the gains mirror about the
/// middle, and no frame is wholly old or wholly new.
#[test]
fn equal_amplitude_sums_to_one_and_mirrors() {
    for len in LENGTHS {
        for k in 0..len {
            let (out, inn) = FadeLaw::EqualAmplitude.gains(k, len);
            assert!((out + inn - 1.0).abs() <= f32::EPSILON, "{k}/{len}");
            // The smootherstep's Horner form is monotone to about 19 ulp
            // near 1, which bounds how exactly the two ends mirror.
            let (mirror_out, mirror_in) = FadeLaw::EqualAmplitude.gains(len - 1 - k, len);
            assert!((out - mirror_in).abs() <= 5e-6, "{k}/{len}");
            assert!((inn - mirror_out).abs() <= 5e-6, "{k}/{len}");
        }
    }
}

/// `EqualPower`'s squares sum to one at every frame, and the gains are the
/// sine/cosine pair of the smootherstep, computed here in `f64`.
#[test]
fn equal_power_is_the_cosine_sine_pair_of_the_smootherstep() {
    for len in LENGTHS {
        for k in 0..len {
            let (out, inn) = FadeLaw::EqualPower.gains(k, len);
            assert!((out * out + inn * inn - 1.0).abs() < 1e-6, "{k}/{len}");
            let angle =
                smootherstep((k + 1) as f64 / (len + 1) as f64) * std::f64::consts::FRAC_PI_2;
            assert!(
                (f64::from(out) - angle.cos()).abs() < 5e-6,
                "{k}/{len}: {out}"
            );
            assert!(
                (f64::from(inn) - angle.sin()).abs() < 5e-6,
                "{k}/{len}: {inn}"
            );
        }
    }
}

/// A fade's position is strictly inside `(0, 1)`: the first frame already
/// hears some of the incoming signal and the last still some of the outgoing,
/// for both laws, at the lengths a 5 ms fade has at 44.1, 48 and 96 kHz. (The
/// first outgoing gain and the last incoming one are within an `f32` of 1.)
#[test]
fn no_frame_of_a_fade_is_wholly_old_or_wholly_new() {
    for rate in [44_100.0, 48_000.0, 96_000.0] {
        let n = declick_frames(SampleRate::of(rate)).get();
        for law in [FadeLaw::EqualAmplitude, FadeLaw::EqualPower] {
            let (out0, in0) = law.gains(0, n);
            let (out_last, in_last) = law.gains(n - 1, n);
            assert!(in0 > 0.0 && out0 <= 1.0, "{law:?} first frame at {rate}");
            assert!(
                out_last > 0.0 && in_last <= 1.0,
                "{law:?} last frame at {rate}"
            );
        }
    }
}

/// `DECLICK` is 5 ms: `round(rate * 0.005)` frames, at least one.
#[test]
fn declick_is_five_milliseconds_in_frames_at_least_one() {
    assert_eq!(DECLICK, Seconds(0.005));
    for (rate, frames) in [
        (44_100.0, 221),
        (48_000.0, 240),
        (96_000.0, 480),
        (192_000.0, 960),
        (100.0, 1),
        (1.0, 1),
    ] {
        assert_eq!(
            declick_frames(SampleRate::of(rate)),
            Samples(frames),
            "{rate}"
        );
    }
}

/// `between` is `from * out + to * in` inside the fade and exactly `to` after.
#[test]
fn between_moves_from_one_value_to_the_other() {
    for len in [1usize, 5, 240] {
        for k in 0..len {
            let (out, inn) = FadeLaw::EqualAmplitude.gains(k, len);
            let want = 3.0 * out + (-2.0) * inn;
            assert_eq!(FadeLaw::EqualAmplitude.between(3.0, -2.0, k, len), want);
        }
        assert_eq!(FadeLaw::EqualPower.between(3.0, -2.0, len, len), -2.0);
        assert_eq!(FadeLaw::EqualPower.between(3.0, -2.0, len + 9, len), -2.0);
    }
}

/// A ramp is the linear piece at `(k + 1) / n`: its last frame is its target
/// exactly, and the frame before it is `n - 1` of `n` of the way.
#[test]
fn a_ramp_lands_on_its_target_at_the_last_frame() {
    for n in [1usize, 2, 3, 7, 64, 240, 4800] {
        assert_eq!(Piece::ramp_at(0.3, -0.7, n - 1, n), -0.7, "n = {n}");
        assert_eq!(Piece::ramp_at(0.3, 0.3, n - 1, n), 0.3, "n = {n}");
    }
    assert_eq!(Piece::ramp_at(0.0, 8.0, 0, 4), 2.0);
    assert_eq!(Piece::ramp_at(0.0, 8.0, 1, 4), 4.0);
    assert_eq!(Piece::ramp_at(0.0, 8.0, 2, 4), 6.0);
    assert_eq!(Piece::ramp_at(8.0, 0.0, 0, 4), 6.0);
}

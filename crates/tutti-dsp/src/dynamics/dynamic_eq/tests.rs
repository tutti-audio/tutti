use core::f64::consts::PI;

use tutti_buffers::ChannelLayout;
use tutti_buffers::{Planes, PlanesMut};
use tutti_units::{CompressionRatio, Db, Hz, Q, SampleRate, Seconds};

use super::*;
use crate::dynamics::curves::upward_gain_lift;
use tutti_buffers::InputOutput;

// The banks under test are `f32` (the session's audio format), so their tolerances are `f32`'s;
// the `f64` banks have their own tests below.

const RATE: SampleRate = SampleRate::SR_48K;
const FS: f64 = 48_000.0;
/// Frames the detector (8 periods of 1 kHz, 384 frames) has settled over many times.
const SETTLE: usize = 12_000;
/// Frames measured at the end: whole periods of every tone the tests use.
const MEASURE: usize = 2_400;

fn sine(f: f64, amplitude: f64, n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| (amplitude * (2.0 * PI * f * i as f64 / FS).sin()) as f32)
        .collect()
}

fn amplitude(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
}

/// The compressor's static curve from its definition (Giannoulis, Massberg and Reiss, 2012,
/// eq. 4 and 5): the reduction in dB, positive, of a level over the threshold.
fn reduction(level: f64, threshold: f64, knee: f64, ratio: f64) -> f64 {
    let slope = 1.0 - 1.0 / ratio;
    if knee <= 0.0 {
        slope * (level - threshold).max(0.0)
    } else if level <= threshold - knee / 2.0 {
        0.0
    } else if level >= threshold + knee / 2.0 {
        slope * (level - threshold)
    } else {
        slope * (level - threshold + knee / 2.0).powi(2) / (2.0 * knee)
    }
}

/// The band's gain in dB at `level` dB: the cut of a downward band, the boost of an upward
/// one (the reduction curve about the threshold, mirrored), held within `range`.
fn band_gain(mode: DynamicMode, level: f64, c: &DynamicEqControls) -> f64 {
    let (t, k, r) = (c.threshold.get(), c.knee.get(), c.ratio.get());
    let range = c.range.get();
    match mode {
        DynamicMode::Downward => -reduction(level, t, k, r).min(range),
        DynamicMode::Upward => reduction(2.0 * t - level, t, k, r).min(range),
    }
}

/// `|H|` in dB at `f` of an RBJ bell of centre `f0`, `q` and `gain` dB at the rate: the
/// analog `1 + k (a^2 - 1) s / (s^2 + k s + 1)`, `k = 1 / (q a)`, `a = 10^(gain / 40)`,
/// on the bilinear map pre-warped at the centre.
fn bell_db(f: f64, f0: f64, q: f64, gain: f64) -> f64 {
    let a = 10f64.powf(gain / 40.0);
    let k = 1.0 / (q * a);
    let t = (PI * f / FS).tan() / (PI * f0 / FS).tan();
    let (dr, di) = (1.0 - t * t, k * t);
    let (nr, ni) = (0.0, k * (a * a - 1.0) * t);
    let den = dr * dr + di * di;
    let (re, im) = (1.0 + (nr * dr + ni * di) / den, (ni * dr - nr * di) / den);
    10.0 * (re * re + im * im).log10()
}

fn controls(threshold: f64, knee: f64, ratio: f64, range: f64, q: f64) -> DynamicEqControls {
    DynamicEqControls {
        threshold: Db::new(threshold),
        knee: Db::new(knee),
        ratio: CompressionRatio::new(ratio),
        range: Db::new(range),
        q: Q::new(q),
        ..DynamicEqControls::DEFAULT
    }
}

fn bank(mode: DynamicMode, width: usize, c: &DynamicEqControls) -> DynamicEqBank<f32> {
    let mut bank = DynamicEqBank::<f32>::new(ChannelLayout::clamped(width), RATE).mode(mode);
    bank.retarget(c);
    bank.jump_to_target();
    bank
}

/// `input` (one slice per channel) through `bank`, keyed by `key`, in calls of `block`.
fn run(
    bank: &mut DynamicEqBank<f32>,
    key: &[&[f32]],
    input: &[&[f32]],
    block: usize,
) -> Vec<Vec<f32>> {
    let n = input[0].len();
    let mut out = vec![vec![0.0f32; n]; input.len()];
    let mut at = 0;
    while at < n {
        let end = (at + block).min(n);
        let k: Vec<&[f32]> = key.iter().map(|c| &c[at..end]).collect();
        let i: Vec<&[f32]> = input.iter().map(|c| &c[at..end]).collect();
        let mut o: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
        bank.process(
            Planes::new(&k).unwrap(),
            &mut InputOutput::paired(Planes::new(&i).unwrap(), PlanesMut::new(&mut o).unwrap())
                .unwrap(),
        )
        .unwrap();
        at = end;
    }
    out
}

/// The gain in dB a sine at the centre is given once settled, from the output over the
/// input across the last [`MEASURE`] frames.
fn measured_gain(mode: DynamicMode, c: &DynamicEqControls, level_db: f64) -> (f64, Db) {
    let x = sine(1000.0, amplitude(level_db), SETTLE);
    let mut b = bank(mode, 1, c);
    let y = run(&mut b, &[&x], &[&x], 480).remove(0);
    let tail = SETTLE - MEASURE;
    (20.0 * (rms(&y[tail..]) / rms(&x[tail..])).log10(), b.gain())
}

/// The settled gain of a sine at the centre is the static curve's: threshold, knee, ratio and
/// range, over levels either side of the threshold and the knee, at two Qs (the band-pass's
/// peak gain of `Q` divided out of the key). 0.1 dB covers the ripple of the detector's mean
/// square (a sine's square, 1% of its mean at 8 periods: under 0.05 dB).
#[test]
fn a_downward_band_settles_on_the_static_curve_at_its_centre() {
    for q in [1.0f64, 4.0] {
        for (threshold, knee, ratio, range) in [
            (-24.0, 0.0, 4.0, 40.0),
            (-12.0, 0.0, 20.0, 6.0),
            (-18.0, 10.0, 2.0, 40.0),
            (-12.0, 8.0, 4.0, 3.0),
        ] {
            for level in [-40.0, -26.0, -20.0, -14.0, -8.0, -2.0] {
                let c = controls(threshold, knee, ratio, range, q);
                let want = band_gain(DynamicMode::Downward, level, &c);
                let (got, held) = measured_gain(DynamicMode::Downward, &c, level);
                assert!(
                    (got - want).abs() < 0.1,
                    "q {q} {c:?} at {level} dB: {got} against the curve's {want}"
                );
                assert!(
                    (held.get() - want).abs() < 0.1,
                    "q {q} {c:?} at {level} dB: the band holds {held:?}, the curve gives {want}"
                );
            }
        }
    }
}

/// An upward band is the curve mirrored about the threshold: a boost under it, flat over it,
/// held within the range. Same tolerance as the downward band.
#[test]
fn an_upward_band_settles_on_the_mirrored_static_curve_at_its_centre() {
    for q in [1.0f64, 4.0] {
        for (threshold, knee, ratio, range) in [
            (-30.0, 0.0, 4.0, 40.0),
            (-20.0, 0.0, 20.0, 6.0),
            (-26.0, 10.0, 2.0, 40.0),
            (-24.0, 8.0, 4.0, 3.0),
        ] {
            for level in [-60.0, -40.0, -34.0, -28.0, -22.0, -12.0, -2.0] {
                let c = controls(threshold, knee, ratio, range, q);
                let want = band_gain(DynamicMode::Upward, level, &c);
                let (got, held) = measured_gain(DynamicMode::Upward, &c, level);
                assert!(
                    (got - want).abs() < 0.1,
                    "q {q} {c:?} at {level} dB: {got} against the curve's {want}"
                );
                assert!(
                    (held.get() - want).abs() < 0.1,
                    "q {q} {c:?} at {level} dB: the band holds {held:?}, the curve gives {want}"
                );
            }
        }
    }
}

/// Below the threshold (and below the knee's lower edge) a downward band is flat: the gain
/// is 0 dB to the bit and the signal passes unchanged (a bell at 0 dB is the identity).
#[test]
fn a_downward_band_is_unity_below_the_threshold() {
    for knee in [0.0, 6.0] {
        let c = controls(-20.0, knee, 8.0, 12.0, 2.0);
        let x = sine(1000.0, amplitude(-30.0), SETTLE);
        let mut b = bank(DynamicMode::Downward, 1, &c);
        let y = run(&mut b, &[&x], &[&x], 256).remove(0);
        assert_eq!(b.gain(), Db::UNITY, "knee {knee}");
        assert!(
            x.iter().zip(&y).all(|(a, b)| a == b),
            "knee {knee}: the signal is not unchanged"
        );
    }
}

/// An upward band over the threshold is likewise flat. (The first milliseconds, before the
/// detector's mean has risen from silence, are under the threshold and boosted; the last
/// [`MEASURE`] frames are past it.)
#[test]
fn an_upward_band_is_unity_above_the_threshold() {
    let c = controls(-30.0, 0.0, 8.0, 12.0, 2.0);
    let x = sine(1000.0, amplitude(-10.0), SETTLE);
    let mut b = bank(DynamicMode::Upward, 1, &c);
    let y = run(&mut b, &[&x], &[&x], 256).remove(0);
    assert_eq!(b.gain(), Db::UNITY);
    let tail = SETTLE - MEASURE;
    assert!(x[tail..].iter().zip(&y[tail..]).all(|(a, b)| a == b));
}

/// A tone away from the centre, with the band cut by a loud key at the centre, is changed by
/// the bell's skirt alone: `|H|` of the RBJ bell at that frequency, Q and gain. The gain is the
/// static curve's for the key (-6 dB over a -20 dB threshold at 4:1 is 10.5 dB). 0.1 dB, as
/// the settled gain's.
#[test]
fn a_tone_off_the_band_is_changed_by_the_bells_skirt_only() {
    let c = controls(-20.0, 0.0, 4.0, 12.0, 2.0);
    let cut = band_gain(DynamicMode::Downward, -6.0, &c);
    assert!((cut + 10.5).abs() < 1e-9);
    let key = sine(1000.0, amplitude(-6.0), SETTLE);
    for f in [100.0, 500.0, 800.0, 1250.0, 2000.0, 4000.0, 12_000.0] {
        let x = sine(f, 0.1, SETTLE);
        let mut b = bank(DynamicMode::Downward, 1, &c);
        let y = run(&mut b, &[&key], &[&x], 480).remove(0);
        let tail = SETTLE - MEASURE;
        let got = 20.0 * (rms(&y[tail..]) / rms(&x[tail..])).log10();
        let want = bell_db(f, 1000.0, 2.0, cut);
        assert!(
            (got - want).abs() < 0.1,
            "{f} Hz: {got} dB against the bell's {want}"
        );
    }
}

/// The off-band tone alone does not move the band: its level through the band-pass at the
/// centre is far under the threshold, so the output is the input.
#[test]
fn a_loud_tone_far_from_the_band_does_not_move_it() {
    let c = controls(-20.0, 0.0, 4.0, 12.0, 4.0);
    let x = sine(8000.0, amplitude(-6.0), SETTLE);
    let mut b = bank(DynamicMode::Downward, 1, &c);
    let y = run(&mut b, &[&x], &[&x], 480).remove(0);
    assert!(
        b.gain().get() > -0.1,
        "a 3-octave-off tone moved the band to {:?}",
        b.gain()
    );
    let tail = SETTLE - MEASURE;
    let got = 20.0 * (rms(&y[tail..]) / rms(&x[tail..])).log10();
    assert!(got.abs() < 0.1, "{got}");
}

/// The attack closes the stated fraction of the way to a steady gain change in its time
/// constant: after `tau` frames `1 - 1/e` of the change. The detector's own mean (8 periods,
/// 384 frames of an attack of 24 000) leaves the figure 1% short of ideal, so 0.03.
#[test]
fn the_attack_reaches_one_minus_inverse_e_in_its_time_constant() {
    let tau = 24_000usize;
    let c = DynamicEqControls {
        attack: Seconds::new(tau as f64 / FS),
        ..controls(-20.0, 0.0, 4.0, 12.0, 1.0)
    };
    let full = band_gain(DynamicMode::Downward, -6.0, &c);
    let x = sine(1000.0, amplitude(-6.0), tau);
    let mut b = bank(DynamicMode::Downward, 1, &c);
    run(&mut b, &[&x], &[&x], 480);
    let fraction = b.gain().get() / full;
    assert!(
        (fraction - (1.0 - (-1.0f64).exp())).abs() < 0.03,
        "after one time constant {fraction}"
    );
}

/// The release falls to `1/e` of the change in its time constant, from a band that was
/// settled at the full change. The detector's mean empties in a few milliseconds of a release
/// of 500: 0.03.
#[test]
fn the_release_falls_to_inverse_e_in_its_time_constant() {
    let tau = 24_000usize;
    let c = DynamicEqControls {
        release: Seconds::new(tau as f64 / FS),
        ..controls(-20.0, 0.0, 4.0, 12.0, 1.0)
    };
    let full = band_gain(DynamicMode::Downward, -6.0, &c);
    let loud = sine(1000.0, amplitude(-6.0), SETTLE);
    let mut b = bank(DynamicMode::Downward, 1, &c);
    run(&mut b, &[&loud], &[&loud], 480);
    assert!((b.gain().get() - full).abs() < 0.1);
    let quiet = vec![0.0f32; tau];
    run(&mut b, &[&quiet], &[&quiet], 480);
    let fraction = b.gain().get() / full;
    assert!(
        (fraction - (-1.0f64).exp()).abs() < 0.03,
        "after one time constant {fraction}"
    );
}

/// One gain serves every channel, from the loudest key channel: a stereo pair with a loud
/// left key and a silent right one is cut as the loud one alone is, on both channels.
#[test]
fn the_gain_is_linked_from_the_loudest_key_channel() {
    let c = controls(-20.0, 0.0, 4.0, 12.0, 1.0);
    let left = sine(1000.0, amplitude(-6.0), SETTLE);
    let right = vec![0.0f32; SETTLE];
    let other = sine(1000.0, 0.1, SETTLE);
    let mut b = bank(DynamicMode::Downward, 2, &c);
    let y = run(&mut b, &[&left, &right], &[&other, &other], 480);
    let want = band_gain(DynamicMode::Downward, -6.0, &c);
    let tail = SETTLE - MEASURE;
    for ch in &y {
        let got = 20.0 * (rms(&ch[tail..]) / rms(&other[tail..])).log10();
        assert!((got - want).abs() < 0.1, "{got} against {want}");
    }
}

/// A finite threshold past what `f32` holds doubled is held, so an upward band at ratio 1
/// lifts nothing rather than reading an overflowed curve as its full range.
#[test]
fn an_upward_band_at_ratio_one_and_a_huge_threshold_lifts_nothing() {
    let c = DynamicEqControls {
        threshold: Db::new(3.0e38),
        ratio: CompressionRatio::new(1.0),
        ..controls(-20.0, 6.0, 4.0, 12.0, 2.0)
    };
    let x = sine(1000.0, 0.5, 4096);
    let mut b = bank(DynamicMode::Upward, 1, &c);
    let y = run(&mut b, &[&x], &[&x], 333).remove(0);
    assert_eq!(b.gain(), Db::UNITY);
    assert_eq!(y, x);
}

/// Controls that are not finite, or below their floors, are the default's or the floor and
/// leave the output finite; a non-finite key leaves it finite and the band recovers.
#[test]
fn hostile_controls_and_keys_leave_the_output_finite() {
    let base = controls(-20.0, 6.0, 4.0, 12.0, 2.0);
    let mut all = vec![
        DynamicEqControls {
            knee: Db::new(-6.0),
            ratio: CompressionRatio::new(0.25),
            range: Db::new(-3.0),
            attack: Seconds::new(-1.0),
            release: Seconds::new(-1.0),
            ..base
        },
        DynamicEqControls {
            threshold: Db::new(3.0e38),
            ratio: CompressionRatio::new(1.0),
            ..base
        },
        DynamicEqControls {
            threshold: Db::new(-3.0e38),
            ..base
        },
        DynamicEqControls {
            range: Db::new(1.0e30),
            knee: Db::new(1.0e30),
            ..base
        },
    ];
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        all.push(DynamicEqControls {
            center: Hz::new(bad),
            q: Q::new(bad),
            threshold: Db::new(bad),
            knee: Db::new(bad),
            ratio: CompressionRatio::new(bad),
            range: Db::new(bad),
            ..base
        });
        all.push(DynamicEqControls {
            attack: Seconds::new(bad),
            release: Seconds::new(bad),
            ..base
        });
    }
    let x = sine(1000.0, 0.5, 4096);
    for c in &all {
        for mode in [DynamicMode::Downward, DynamicMode::Upward] {
            let mut b = bank(mode, 1, c);
            let y = run(&mut b, &[&x], &[&x], 333).remove(0);
            assert!(y.iter().all(|v| v.is_finite()), "{c:?} {mode:?}");
            assert!(b.gain().get().is_finite(), "{c:?} {mode:?}");
        }
    }
    let mut key = x.clone();
    key[100] = f32::NAN;
    key[200] = f32::INFINITY;
    key[300] = f32::NEG_INFINITY;
    for mode in [DynamicMode::Downward, DynamicMode::Upward] {
        let mut b = bank(mode, 1, &base);
        let y = run(&mut b, &[&key], &[&x], 333).remove(0);
        assert!(y.iter().all(|v| v.is_finite()), "{mode:?}");
        assert!(b.gain().get().is_finite(), "{mode:?}");
    }
}

/// A reset is flat: the gain change is gone and the filters are silent.
#[test]
fn a_reset_band_is_flat() {
    let c = controls(-20.0, 0.0, 4.0, 12.0, 1.0);
    let x = sine(1000.0, amplitude(-6.0), SETTLE);
    let mut b = bank(DynamicMode::Downward, 1, &c);
    run(&mut b, &[&x], &[&x], 480);
    assert!(b.gain().get() < -9.0);
    b.reset();
    assert_eq!(b.gain(), Db::UNITY);
    let silent = vec![0.0f32; 64];
    let y = run(&mut b, &[&silent], &[&silent], 64).remove(0);
    assert!(y.iter().all(|&v| v == 0.0));
}

/// The mirror, pinned by hand: 10 dB under a -30 dB threshold at 4:1 and a hard knee lifts
/// 10 * (1 - 1/4) = 7.5 dB; at the threshold a 10 dB knee lifts `slope (knee / 2)^2 / (2
/// knee)` = 0.75 * 25 / 20 = 0.9375 dB (the reduction's knee term, the same at the centre).
#[test]
fn the_upward_lift_has_its_hand_computed_anchors() {
    let lift = |level: f64, knee: f64| upward_gain_lift(level, -30.0, 4.0, knee);
    assert!((lift(-40.0, 0.0) - 7.5).abs() < 1e-14);
    assert!((lift(-30.0, 10.0) - 0.9375).abs() < 1e-14);
    assert_eq!(
        lift(-20.0, 0.0),
        0.0,
        "over the threshold nothing is lifted"
    );
}

/// The render does not depend on how the stream is cut into calls: blocks of 1, 7, 16 and
/// 480 frames give the same bits, in both modes, with the times running and the key a
/// different signal from the input.
#[test]
fn the_render_is_the_same_bits_in_any_blocks() {
    let n = 4_800;
    let input: Vec<f32> = sine(300.0, 0.4, n)
        .iter()
        .zip(sine(5_000.0, 0.2, n))
        .map(|(a, b)| a + b)
        .collect();
    let mut key = sine(1_000.0, 0.5, n);
    key[n / 2..].iter_mut().for_each(|x| *x *= 0.05);
    let c = DynamicEqControls {
        attack: Seconds::new(0.002),
        release: Seconds::new(0.03),
        ..controls(-24.0, 6.0, 4.0, 12.0, 2.0)
    };
    for mode in [DynamicMode::Downward, DynamicMode::Upward] {
        let whole = run(&mut bank(mode, 1, &c), &[&key], &[&input], n).remove(0);
        assert!(whole.iter().zip(&input).any(|(a, b)| a != b), "{mode:?}");
        for block in [1, 7, 16, 480] {
            let got = run(&mut bank(mode, 1, &c), &[&key], &[&input], block).remove(0);
            assert_eq!(got, whole, "{mode:?} in blocks of {block}");
        }
    }
}

/// A band whose range is 0 dB never leaves flat, however far over its threshold the key is,
/// so an `f64` bell at 0 dB returns a signal of more than 24 bits unchanged: the flat bell
/// passes the input (`m0 = 1`, `m1 = m2 = 0`) and nothing in the path narrows it.
#[test]
fn an_f64_band_with_zero_range_is_the_identity() {
    use tutti_fixtures::signal::{assert_close_f64, wide_signal};
    let x = wide_signal(4_800);
    for mode in [DynamicMode::Downward, DynamicMode::Upward] {
        let mut b = DynamicEqBank::<f64>::new(ChannelLayout::MONO, RATE).mode(mode);
        b.retarget(&controls(-60.0, 0.0, 4.0, 0.0, 1.0));
        b.jump_to_target();
        let mut out = vec![0.0f64; x.len()];
        b.process(
            Planes::new(&[&x[..]]).unwrap(),
            &mut InputOutput::paired(
                Planes::new(&[&x[..]]).unwrap(),
                PlanesMut::new(&mut [&mut out[..]]).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        assert_close_f64(&out, &x, 0.0);
    }
}

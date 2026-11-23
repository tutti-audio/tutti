use core::f64::consts::PI;

use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_buffers::{Mono, Planes, PlanesMut};
use tutti_units::{CompressionRatio, Seconds};

use super::*;
use crate::{Complex32, Complex64, Lti, RealFft};
use tutti_buffers::InputOutput;
use tutti_fixtures::signal::{amplitude_at, lock_in};
use tutti_units::Hz;
use tutti_units::SampleRate;

// The banks under test are `f32` (the session's audio format), so their tolerances are `f32`'s;
// the `f64` banks have their own tests below.

const RATE: SampleRate = SampleRate::SR_48K;
/// Frames a ramp takes at [`RATE`]: `ceil(0.005 * 48000)`.
const RAMP: usize = 240;
/// Frames for the crossover's sections and the followers to settle on a constant.
const SETTLE: usize = 24_000;

fn bank(width: u16, bands: usize) -> MultibandBank<f32> {
    MultibandBank::<f32>::new(
        BankShape::new(
            ChannelTopology::default_for(ChannelLayout::from_count(width)).unwrap(),
            MultibandBands::new(u8::try_from(bands).unwrap()).unwrap(),
        ),
        RATE,
    )
}

fn hz(freqs: &[f64]) -> CrossoverControls {
    let mut frequencies = [Hz::ZERO; 5];
    for (f, &x) in frequencies.iter_mut().zip(freqs) {
        *f = Hz::new(x);
    }
    CrossoverControls { frequencies }
}

fn band(
    threshold: f64,
    knee: f64,
    ratio: f64,
    attack: f64,
    release: f64,
    makeup: f64,
) -> BandSettings {
    BandSettings::new().compressor(CompressorControls {
        threshold: Db::new(threshold),
        knee: Db::new(knee),
        ratio: CompressionRatio::new(ratio),
        attack: Seconds::new(attack),
        release: Seconds::new(release),
        makeup: Db::new(makeup),
        ..CompressorControls::DEFAULT
    })
}

/// Every band at unity ratio and no makeup, the splits at `freqs`: nothing is reduced.
fn unity(freqs: &[f64]) -> MultibandSettings {
    MultibandSettings::new()
        .crossover(hz(freqs))
        .bands([band(-18.0, 0.0, 1.0, 0.01, 0.1, 0.0); MAX_BANDS])
}

/// `audio` (one slice per channel) through `k` in calls of `chunk` frames, band `b`'s
/// threshold control signal `signals[b]` where given.
fn run_in<F: Float + SimdLanes + Lerp>(
    k: &mut MultibandBank<F>,
    audio: &[&[F]],
    signals: [Option<&[F]>; MAX_BANDS],
    chunk: usize,
) -> Vec<Vec<F>> {
    let n = audio[0].len();
    let mut out = vec![vec![F::ZERO; n]; audio.len()];
    let mut at = 0;
    while at < n {
        let end = (at + chunk).min(n);
        let a: Vec<&[F]> = audio.iter().map(|c| &c[at..end]).collect();
        let mut outs: Vec<&mut [F]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
        let mut driven = MultibandSignals::default();
        for (slot, signal) in driven.thresholds.iter_mut().zip(signals) {
            *slot = signal.map(|l| Mono::new(&l[at..end]));
        }
        k.process(
            driven,
            &mut InputOutput::paired(Planes::new(&a).unwrap(), PlanesMut::new(&mut outs).unwrap())
                .unwrap(),
        )
        .unwrap();
        at = end;
    }
    out
}

fn run<F: Float + SimdLanes + Lerp>(k: &mut MultibandBank<F>, audio: &[&[F]]) -> Vec<Vec<F>> {
    run_in(k, audio, [None; MAX_BANDS], 256)
}

fn mean<F: Float>(x: &[F]) -> f64 {
    x.iter().map(|&v| -> f64 { v.into() }).sum::<f64>() / x.len() as f64
}

fn tone(freq: f64, amplitude: f32, len: usize) -> Vec<f32> {
    (0..len)
        .map(|n| amplitude * libm::sin(2.0 * PI * freq * n as f64 / RATE.get()) as f32)
        .collect()
}

/// The reduction in dB of the static curve at a hard knee, from its definition.
fn hard(level_db: f64, threshold: f64, ratio: f64) -> f64 {
    (1.0 - 1.0 / ratio) * (level_db - threshold).max(0.0)
}

/// The reduction in dB of the static curve with a knee of `knee` dB (Giannoulis, Massberg
/// and Reiss, 2012, eq. 4): nothing below `T - W/2`, the quadratic inside the knee, the
/// hard-knee line above `T + W/2`.
fn curve(level: f64, threshold: f64, ratio: f64, knee: f64) -> f64 {
    let over = level - threshold;
    if 2.0 * over < -knee {
        0.0
    } else if 2.0 * over.abs() <= knee {
        (1.0 - 1.0 / ratio) * (over + knee / 2.0).powi(2) / (2.0 * knee)
    } else {
        (1.0 - 1.0 / ratio) * over
    }
}

fn db(amplitude: f64) -> f64 {
    20.0 * libm::log10(amplitude)
}

fn linear(db: f64) -> f64 {
    libm::pow(10.0, db / 20.0)
}

/// The warped frequency ratio `tan(pi f / fs) / tan(pi fc / fs)` of the bilinear transform.
fn warp(f: f64, fc: f64) -> f64 {
    libm::tan(PI * f / RATE.get()) / libm::tan(PI * fc / RATE.get())
}

/// The magnitude of band `band` of a tree with splits `fc` at `f`: its low-pass `1 / (1 +
/// u^4)` (the last band has none), the high-pass `u^4 / (1 + u^4)` of every split below it,
/// and the all-passes of the later splits, which are unity.
fn band_magnitude(band: usize, fc: &[f64], f: f64) -> f64 {
    let hp = |c: f64| warp(f, c).powi(4) / (1.0 + warp(f, c).powi(4));
    let lp = |c: f64| 1.0 / (1.0 + warp(f, c).powi(4));
    let below: f64 = fc[..band].iter().map(|&c| hp(c)).product();
    below * fc.get(band).map_or(1.0, |&c| lp(c))
}

/// The all-pass of a split at `fc` at `f`: the 2nd-order Butterworth
/// `(1 - u^2 - j sqrt2 u) / (1 - u^2 + j sqrt2 u)`, `u = tan(pi f / fs) / tan(pi fc / fs)`.
fn all_pass(f: f64, fc: f64) -> Complex64 {
    let u = warp(f, fc);
    let (re, im) = (1.0 - u * u, core::f64::consts::SQRT_2 * u);
    let d = re * re + im * im;
    // (re - j im) / (re + j im)
    Complex64::new((re * re - im * im) / d, -2.0 * re * im / d)
}

/// The analytic formulas the tests lean on are themselves right: the bands' magnitudes add to
/// 1 at every frequency (the bands are phase-aligned and sum to an all-pass), the all-pass is
/// unit, and the design's own model reads a split's band at -6.02 dB.
#[test]
fn the_analytic_band_magnitudes_sum_to_one() {
    let fc = [120.0, 1_000.0, 6_000.0];
    for f in [
        20.0, 100.0, 120.0, 500.0, 1_000.0, 3_000.0, 6_000.0, 15_000.0,
    ] {
        let sum: f64 = (0..4).map(|b| band_magnitude(b, &fc, f)).sum();
        assert!((sum - 1.0).abs() < 1e-12, "{f} Hz: {sum}");
        assert!((all_pass(f, 1_000.0).norm() - 1.0).abs() < 1e-12);
    }
    let m = crate::testing::crossover_model(&[Hz::new(1_000.0)], RATE);
    let mut g = [Complex64::new(0.0, 0.0)];
    m.frequency_response(0, &[Hz::new(1_000.0)], &mut g);
    assert!((g[0].norm() - band_magnitude(0, &[1_000.0], 1_000.0)).abs() < 1e-6);
}

/// `MultibandBands` keeps the range-checked shape over `2..=4`, the crossover design's band
/// counts.
#[test]
fn the_band_count_keeps_the_range_checked_shape() {
    tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
        new: MultibandBands::new,
        get: MultibandBands::get,
        of: Some(MultibandBands::of),
        valid: &[2, 3, 4],
        invalid: &[0, 1, 5, 6, 255],
    });
}

/// A band count of one or five is not a count: `MultibandBands::new` refuses both ends of
/// the neighbourhood of the range.
#[test]
fn a_band_count_one_below_or_above_the_range_is_refused() {
    assert_eq!(MultibandBands::new(1), None);
    assert_eq!(MultibandBands::new(5), None);
}

/// A bank has the band count it was built with, over any width a bus holds, the empty bus
/// included.
#[test]
fn a_bank_has_the_bands_it_was_built_with_over_a_width() {
    for bands in 2..=4 {
        assert_eq!(bank(2, bands).bands(), bands);
    }
    assert_eq!(bank(0, 3).bands(), 3);
}

/// With every band at unity ratio and no makeup the bands sum to the product of the splits'
/// Butterworth all-passes, in magnitude and in phase: the output's complex response at a tone
/// is read by lock-in over whole periods and compared to the analytic response, for 2, 3 and
/// 4 bands.
#[test]
fn unity_bands_sum_to_the_analytic_all_pass_response() {
    // Whole periods of every tone on the 5 Hz grid.
    const WINDOW: usize = 9_600;
    for (bands, freqs) in [
        (2, vec![1_000.0f64]),
        (3, vec![200.0, 3_000.0]),
        (4, vec![120.0, 1_000.0, 6_000.0]),
    ] {
        let mut turns = 0.0f64;
        for i in 0..14 {
            let f = (30.0 * (18_000.0f64 / 30.0).powf(f64::from(i) / 13.0) / 5.0).round() * 5.0;
            let mut k = bank(1, bands);
            k.retarget(&unity(&freqs));
            let x = tone(f, 1.0, SETTLE + WINDOW);
            let out = run(&mut k, &[&x]).remove(0);
            let (xr, xi) = lock_in(&x[SETTLE..], Hz::new(f), RATE);
            let (yr, yi) = lock_in(&out[SETTLE..], Hz::new(f), RATE);
            let got = Complex64::new(yr, yi) / Complex64::new(xr, xi);
            let want = freqs
                .iter()
                .map(|&c| all_pass(f, c))
                .fold(Complex64::new(1.0, 0.0), |a, b| a * b);
            assert!(
                (got - want).norm() < 3e-5,
                "{bands} bands at {f} Hz: {got:?} vs {want:?}"
            );
            turns = turns.max((want - Complex64::new(1.0, 0.0)).norm());
        }
        assert!(
            turns > 1.0,
            "{bands} bands: the sum is no input copy, its phase turns"
        );
    }
}

/// The sum of an impulse is flat in magnitude at every bin (the all-pass of the crossover),
/// read from the whole spectrum of one render.
#[test]
fn unity_bands_have_a_flat_impulse_magnitude() {
    const N: usize = 1 << 15;
    let mut impulse = vec![0.0f32; N];
    impulse[0] = 1.0;
    for (bands, freqs) in [
        (2, vec![1_000.0f64]),
        (3, vec![200.0, 3_000.0]),
        (4, vec![120.0, 1_000.0, 6_000.0]),
    ] {
        let mut k = bank(1, bands);
        k.retarget(&unity(&freqs));
        let out = run(&mut k, &[&impulse]).remove(0);
        let fft = RealFft::new(N);
        let mut scratch = fft.new_scratch();
        let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
        fft.forward(Mono::new(&out), &mut bins, &mut scratch);
        for (i, b) in bins.iter().enumerate().skip(1) {
            let magnitude = f64::from(b.re).hypot(f64::from(b.im));
            assert!(
                (magnitude - 1.0).abs() < 2e-4,
                "{bands} bands, bin {i}: {magnitude}"
            );
        }
    }
}

/// A steady tone inside one band settles that band's gain on the compressor's static curve,
/// and leaves every other band alone.
///
/// The compressor holds the deepest reduction (an instant attack, a release of infinity), so
/// the settled reduction is the curve at the band's peak level, `A |H_b(f)|` with `|H_b|` the
/// band's analytic magnitude, whatever the ripple of the band's rectified tone. The tone runs
/// at ratio 1 until the crossover has settled, so its start-up overshoot is not held. The
/// meter reads that reduction, and the output amplitude is `A` times the bands' magnitudes
/// weighted by their gains (the bands are phase-aligned, so their amplitudes add). Each band
/// takes a hard knee, a level inside the knee and a level under it, and a makeup.
#[test]
fn a_tone_in_a_band_settles_on_that_bands_static_curve() {
    // Whole periods of every tone in 2 s, over a dense set of sample phases.
    const WINDOW: usize = 96_000;
    let splits = [200.0f64, 1_000.0, 5_000.0];
    let tones = [100.5, 500.5, 2_500.5, 10_000.5];
    for (b, &f) in tones.iter().enumerate() {
        for (knee, over, ratio, makeup) in [
            (0.0, 8.0, 3.0, 0.0),
            (6.0, 1.0, 4.0, 2.0),
            (6.0, -2.0, 4.0, -3.0),
        ] {
            let a = 0.5f64;
            let level = db(a * band_magnitude(b, &splits, f));
            let threshold = level - over;
            let mut k = bank(1, 4);
            k.retarget(&unity(&splits));
            let x = tone(f, a as f32, SETTLE + 4_800 + WINDOW);
            run(&mut k, &[&x[..SETTLE]]);
            k.take_reading();
            let mut set = unity(&splits);
            set.bands[b] = band(threshold, knee, ratio, 0.0, f64::INFINITY, makeup);
            k.retarget(&set);
            let out = run(&mut k, &[&x[SETTLE..]]).remove(0);
            let reading = k.take_reading();

            let want = curve(level, f64::from(threshold as f32), ratio, knee);
            let got = reading.band(b).unwrap().get();
            assert!(
                (got - want).abs() < 2e-3,
                "band {b}, knee {knee}: metered {got} dB, the curve says {want}"
            );
            for other in (0..4).filter(|&o| o != b) {
                assert_eq!(
                    reading.band(other).unwrap(),
                    Db::UNITY,
                    "band {b} leaves band {other} alone"
                );
            }
            let gain = linear(makeup - want);
            let weighted: f64 = (0..4)
                .map(|j| band_magnitude(j, &splits, f) * if j == b { gain } else { 1.0 })
                .sum();
            let amp = amplitude_at(&out[4_800..], Hz::new(f), RATE);
            assert!(
                (amp - a * weighted).abs() < 2e-5,
                "band {b}, knee {knee}: output {amp} vs {}",
                a * weighted
            );
        }
    }
}

/// Loud content in band 0 is reduced by the compressor's static curve and a quiet band is
/// left alone.
///
/// A constant 0.5 (-6.02 dB) is all band 0 once the crossover settles, and the one input
/// whose detector reading is exact: at -20 dB, 4:1 the reduction is `(-6.02 + 20) * 3/4` dB,
/// with a 2 dB makeup on it. The tone is 0.01 (-40 dB), under every band's -18 dB threshold.
#[test]
fn a_loud_band_follows_the_static_curve_and_a_quiet_band_is_left_alone() {
    let mut set = unity(&[200.0, 3_000.0]);
    set.bands[0] = band(-20.0, 0.0, 4.0, 0.001, 0.001, 2.0);
    let len = SETTLE + 9_600;
    let x: Vec<f32> = tone(5_000.0, 0.01, len).iter().map(|s| 0.5 + s).collect();
    let mut k = bank(1, 3);
    k.retarget(&set);
    let mut out = run(&mut k, &[&x[..SETTLE]]);
    k.take_reading();
    out[0].extend_from_slice(&run(&mut k, &[&x[SETTLE..]])[0]);
    let reduction = hard(db(0.5), -20.0, 4.0);
    let want = 0.5 * linear(2.0 - reduction);
    let dc = mean(&out[0][SETTLE..]);
    assert!(
        (dc - want).abs() < 5e-6 * want,
        "band 0 settled at {dc}, the curve says {want}"
    );
    let amp = amplitude_at(&out[0][SETTLE..], Hz::new(5_000.0), RATE);
    assert!((amp - 0.01).abs() < 2e-5, "the quiet band is {amp}");
    let reading = k.take_reading();
    let deepest = reading.band(0).unwrap().get();
    assert!(
        (deepest - reduction).abs() < 1e-3,
        "{deepest} vs {reduction}"
    );
    assert!(reading.band(1).unwrap().get() < 1e-3 && reading.band(2).unwrap().get() < 1e-3);
}

/// A bypassed band passes unreduced and without makeup, and meters nothing; releasing the
/// bypass compresses it again.
#[test]
fn a_bypassed_band_passes_unprocessed() {
    let mut set = unity(&[200.0, 3_000.0]);
    set.bands[0] = band(-30.0, 0.0, 8.0, 0.001, 0.001, 6.0);
    set.bands[0].bypass = true;
    let mut k = MultibandBank::<f64>::new(
        BankShape::new(ChannelTopology::MONO, MultibandBands::of(3)),
        RATE,
    );
    k.retarget(&set);
    let x = vec![0.5f64; SETTLE + 9_600];
    let out = run(&mut k, &[&x]);
    assert!((mean(&out[0][SETTLE..]) - 0.5).abs() < 1e-12);
    assert_eq!(k.take_reading().band(0).unwrap(), Db::UNITY);
    set.bands[0].bypass = false;
    k.retarget(&set);
    let out = run(&mut k, &[&x]);
    // Over -30 dB at 8:1 the 0.5 DC is reduced by `(30 - 6.02) * 7/8`, with the makeup.
    let want = 0.5 * linear(6.0 - hard(db(0.5), -30.0, 8.0));
    assert!(
        (mean(&out[0][SETTLE..]) - want).abs() < 1e-9,
        "{}",
        mean(&out[0][SETTLE..])
    );
}

/// While any band is soloed the output is the sum of the soloed bands alone: their analytic
/// magnitudes at the tone, which add because the bands are phase-aligned.
#[test]
fn solo_leaves_only_the_soloed_bands() {
    let freqs = [300.0f64, 3_000.0];
    let (tone_hz, len) = (3_500.0f64, SETTLE + 9_600);
    let x = tone(tone_hz, 1.0, len);
    for soloed in [
        [true, false, false],
        [false, true, false],
        [true, true, false],
        [false, false, true],
    ] {
        let mut set = unity(&freqs);
        for (b, &s) in set.bands.iter_mut().zip(&soloed) {
            b.solo = s;
        }
        let want: f64 = (0..3)
            .filter(|&b| soloed[b])
            .map(|b| band_magnitude(b, &freqs, tone_hz))
            .sum();
        let mut k = bank(1, 3);
        k.retarget(&set);
        let out = run(&mut k, &[&x]);
        let got = amplitude_at(&out[0][SETTLE..], Hz::new(tone_hz), RATE);
        assert!((got - want).abs() < 1e-5, "{soloed:?}: {got} vs {want}");
    }
}

/// A bypass that flips ramps linearly over 5 ms and lands: on a settled DC the output is the
/// blend `g (1 - w) + w` of the compressed gain `g` and unity, `w` the weight `k / 240` at
/// frame `k - 1` of the ramp.
#[test]
fn a_moved_bypass_ramps_over_five_milliseconds() {
    let mut set = unity(&[200.0, 3_000.0]);
    set.bands[0] = band(-30.0, 0.0, 8.0, 0.001, 0.001, 0.0);
    let mut k = bank(1, 3);
    k.retarget(&set);
    let x = vec![0.5f32; SETTLE + 2 * RAMP];
    let before = run(&mut k, &[&x[..SETTLE]]);
    let held = f64::from(*before[0].last().unwrap());
    set.bands[0].bypass = true;
    k.retarget(&set);
    let out = run(&mut k, &[&x[SETTLE..]])[0].clone();
    for (j, &y) in out.iter().take(RAMP).enumerate() {
        let w = (j + 1) as f64 / RAMP as f64;
        let want = held * (1.0 - w) + 0.5 * w;
        assert!(
            (f64::from(y) - want).abs() < 2e-4,
            "frame {j}: {y} vs {want}"
        );
    }
    for (j, &y) in out.iter().enumerate().skip(RAMP) {
        assert!((y - 0.5).abs() < 1e-5, "frame {j} after the ramp: {y}");
    }
    assert!(
        held < 0.45,
        "the band was compressing ({held}), or this proves nothing"
    );
}

/// A solo that moves mutes the other band over 5 ms: the muted band's DC is its settled level
/// times the audible weight `1 - k / 240`, and lands on silence.
#[test]
fn a_moved_solo_ramps_the_mute_over_five_milliseconds() {
    let set = unity(&[200.0, 3_000.0]);
    let mut k = bank(1, 3);
    k.retarget(&set);
    let x = vec![0.5f32; SETTLE + 2 * RAMP];
    let before = run(&mut k, &[&x[..SETTLE]]);
    let held = f64::from(*before[0].last().unwrap());
    let mut soloed = set;
    soloed.bands[1].solo = true;
    k.retarget(&soloed);
    let out = run(&mut k, &[&x[SETTLE..]])[0].clone();
    for (j, &y) in out.iter().take(RAMP).enumerate() {
        let w = (j + 1) as f64 / RAMP as f64;
        let want = held * (1.0 - w);
        assert!(
            (f64::from(y) - want).abs() < 2e-4,
            "frame {j}: {y} vs {want}"
        );
    }
    for (j, &y) in out.iter().enumerate().skip(RAMP) {
        assert!(y.abs() < 1e-5, "frame {j} after the ramp: {y}");
    }
    assert!((held - 0.5).abs() < 1e-5, "{held}");
}

/// Detection is linked across channels: one reduction from the loudest channel of the band,
/// applied to both, whichever channel is the loud one.
#[test]
fn the_linked_gain_follows_the_loudest_channel() {
    let mut set = unity(&[200.0, 3_000.0]);
    set.bands[0] = band(-20.0, 0.0, 4.0, 0.001, 0.001, 0.0);
    let len = SETTLE + 4_800;
    let (loud, quiet) = (vec![0.5f32; len], vec![0.05f32; len]);
    let g = linear(-hard(db(0.5), -20.0, 4.0));
    for (left, right, loud_side) in [(&loud, &quiet, 0), (&quiet, &loud, 1)] {
        let mut k = bank(2, 3);
        k.retarget(&set);
        let out = run(&mut k, &[left, right]);
        let (l, r) = (mean(&out[0][SETTLE..]), mean(&out[1][SETTLE..]));
        let (want_l, want_r) = if loud_side == 0 {
            (0.5, 0.05)
        } else {
            (0.05, 0.5)
        };
        // The quiet channel alone is under the threshold (-26 dB) and would not be reduced.
        assert!(
            (l - want_l * g).abs() < 1e-5 * g,
            "loud {loud_side}: left {l}"
        );
        assert!(
            (r - want_r * g).abs() < 1e-5 * g,
            "loud {loud_side}: right {r}"
        );
    }
}

/// A band's follower is the compressor's one-pole: after one attack time on a settled level
/// the reduction has closed `1 - 1/e` of its way to the curve.
///
/// The level is a settled constant and the step is the threshold control signal dropping from +20 dB
/// (nothing to reduce) to -30 dB, so the target reduction jumps at once and the crossover
/// has no transient of its own to blur it.
#[test]
fn a_bands_follower_closes_one_minus_one_over_e_in_one_attack() {
    let mut set = unity(&[200.0, 3_000.0]);
    let attack = 0.01;
    set.bands[0] = band(0.0, 0.0, 8.0, attack, 1.0, 0.0);
    let mut k = bank(1, 3);
    k.retarget(&set);
    let high = vec![20.0f32; SETTLE];
    run_in(
        &mut k,
        &[&vec![0.5f32; SETTLE]],
        [Some(&high), None, None, None],
        256,
    );
    k.take_reading();
    let frames = (attack * RATE.get()) as usize;
    let low = vec![-30.0f32; frames];
    run_in(
        &mut k,
        &[&vec![0.5f32; frames]],
        [Some(&low), None, None, None],
        256,
    );
    let reached = k.take_reading().band(0).unwrap().get();
    let curve = hard(db(0.5), -30.0, 8.0);
    let want = curve * (1.0 - 1.0 / core::f64::consts::E);
    assert!((reached - want).abs() < 2e-3 * curve, "{reached} vs {want}");
}

/// The meter holds the deepest reduction since it was last taken, over every call in between
/// and not the last frame's, and the read starts again from nothing.
#[test]
fn the_meter_reports_the_deepest_reduction_since_the_last_read() {
    let mut set = unity(&[200.0, 3_000.0]);
    set.bands[0] = band(-30.0, 0.0, 8.0, 0.001, 0.005, 0.0);
    let mut k = bank(1, 3);
    k.retarget(&set);
    run(&mut k, &[&vec![0.5f32; SETTLE]]);
    k.take_reading();
    run(&mut k, &[&vec![0.5f32; 4_800]]);
    run(&mut k, &[&vec![0.0f32; 4_800]]);
    let curve = hard(db(0.5), -30.0, 8.0);
    let both = k.take_reading().band(0).unwrap().get();
    assert!((both - curve).abs() < 1e-3, "{both} vs {curve}");
    assert_eq!(k.take_reading().band(0).unwrap(), Db::UNITY);
    // The follower released during the silence: nothing is held over from before the read.
    run(&mut k, &[&vec![0.0f32; 4_800]]);
    assert!(k.take_reading().band(0).unwrap().get() < 1e-3);
    // Past the last band there is nothing to read.
    assert_eq!(k.take_reading().band(3), None);
}

/// Each band reads its own threshold control signal per frame: driving band 0's control signal low reduces band
/// 0 and no other, a frame that is not finite falls back to the set threshold, and a control signal
/// in `f64` reads as the same dB.
#[test]
fn a_band_reads_its_own_threshold_signal() {
    let mut set = unity(&[200.0, 3_000.0]);
    for b in &mut set.bands {
        *b = band(0.0, 0.0, 4.0, 0.001, 0.05, 0.0);
    }
    let len = SETTLE;
    let x = vec![0.5f32; len];
    let low = vec![-30.0f32; len];
    let mut k = bank(1, 3);
    k.retarget(&set);
    run_in(&mut k, &[&x], [Some(&low), None, None, None], 256);
    let want = hard(db(0.5), -30.0, 4.0);
    let reading = k.take_reading();
    let d0 = reading.band(0).unwrap().get();
    assert!((d0 - want).abs() < 0.05, "{d0} vs {want}");
    assert!(reading.band(1).unwrap().get() < 1e-3 && reading.band(2).unwrap().get() < 1e-3);
    // A control signal that is NaN reads as the set threshold (0 dB): nothing to reduce.
    let nan = vec![f32::NAN; len];
    let mut k = bank(1, 3);
    k.retarget(&set);
    run_in(&mut k, &[&x], [Some(&nan), None, None, None], 256);
    assert_eq!(k.take_reading().band(0).unwrap(), Db::UNITY);
    // The same control signal in f64 over f64 audio.
    let mut k = MultibandBank::<f64>::new(
        BankShape::new(ChannelTopology::MONO, MultibandBands::of(3)),
        RATE,
    );
    k.retarget(&set);
    let (x64, low64) = (vec![0.5f64; len], vec![-30.0f64; len]);
    let mut out = vec![0.0f64; len];
    let mut signals = MultibandSignals::<f64>::default();
    signals.thresholds[0] = Some(Mono::new(&low64));
    let planes = [&x64[..]];
    let mut outs = [&mut out[..]];
    k.process(
        signals,
        &mut InputOutput::paired(
            Planes::new(&planes).unwrap(),
            PlanesMut::new(&mut outs).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let d0 = k.take_reading().band(0).unwrap().get();
    assert!((d0 - want).abs() < 0.05, "f64: {d0} vs {want}");
}

/// Any split of a stream into calls gives the same bits: the controls move at fixed frames
/// (a split, a ratio, a makeup, a bypass and a solo), the audio is wide, a threshold control signal is
/// driven, and the calls are every length from one frame to the whole stream.
#[test]
fn any_call_length_gives_the_same_bits() {
    let len = 2_000;
    let audio: Vec<Vec<f32>> = (0..3)
        .map(|c| {
            (0..len)
                .map(|n| {
                    0.8 * libm::sin(0.037 * (c + 1) as f64 * n as f64) as f32
                        + 0.1 * libm::cos(1.31 * n as f64) as f32
                })
                .collect()
        })
        .collect();
    let signal: Vec<f32> = (0..len).map(|n| -40.0 + (n % 97) as f32 * 0.2).collect();
    let mut first = unity(&[300.0, 2_000.0, 8_000.0]);
    first.bands[0] = band(-30.0, 6.0, 4.0, 0.002, 0.02, 3.0);
    first.bands[2] = band(-25.0, 0.0, 2.0, 0.001, 0.05, -2.0);
    let mut second = first;
    second.crossover = hz(&[150.0, 3_000.0, 9_000.0]);
    second.bands[0].compressor.ratio = CompressionRatio::new(8.0);
    second.bands[2].compressor.makeup = Db::new(4.0);
    second.bands[1].bypass = true;
    second.bands[3].solo = true;
    // Aims at frames 333 and 1_001, whichever the calls are.
    let cuts = [(0, first), (333, second), (1_001, first)];
    let render = |chunk: usize| {
        let mut k = bank(3, 4);
        let mut out = vec![vec![0.0f32; len]; 3];
        for (i, &(at, set)) in cuts.iter().enumerate() {
            let end = cuts.get(i + 1).map_or(len, |c| c.0);
            k.retarget(&set);
            let a: Vec<&[f32]> = audio.iter().map(|c| &c[at..end]).collect();
            let got = run_in(
                &mut k,
                &a,
                [Some(&signal[at..end]), None, None, None],
                chunk,
            );
            for (o, g) in out.iter_mut().zip(got) {
                o[at..end].copy_from_slice(&g);
            }
        }
        out
    };
    let whole = render(len);
    assert!(
        whole.iter().flatten().all(|y| y.is_finite()) && whole[0].iter().any(|&y| y != 0.0),
        "the stream is not silence"
    );
    for chunk in [1, 7, 63, 64, 65, 100, 128, 129, 500] {
        let got = render(chunk);
        for (c, (g, w)) in got.iter().zip(&whole).enumerate() {
            assert!(
                g.iter().zip(w).all(|(a, b)| a.to_bits() == b.to_bits()),
                "calls of {chunk} frames differ on channel {c}"
            );
        }
    }
}

/// A reset bank is a fresh one: after a loud, moved and bypassed run, `reset` and the same
/// retarget render the bits a fresh bank renders, and so does `process` straight after `reset`
/// with no `retarget`; the meters read nothing.
#[test]
fn a_reset_bank_renders_as_a_fresh_one() {
    let len = 3_000;
    let x: Vec<f32> = (0..len)
        .map(|n| 0.7 * libm::sin(0.05 * n as f64) as f32)
        .collect();
    let mut loud = unity(&[200.0, 3_000.0]);
    loud.bands[0] = band(-40.0, 0.0, 10.0, 0.001, 0.2, 6.0);
    loud.bands[1].bypass = true;
    loud.bands[2].solo = true;
    let mut set = unity(&[400.0, 2_000.0]);
    set.bands[1] = band(-20.0, 3.0, 3.0, 0.001, 0.05, 2.0);
    let mut fresh = bank(1, 3);
    fresh.retarget(&set);
    let want = run(&mut fresh, &[&x]);
    let mut k = bank(1, 3);
    k.retarget(&loud);
    run(&mut k, &[&vec![0.9f32; 5_000]]);
    k.reset();
    assert_eq!(k.take_reading().iter().map(Db::get).sum::<f64>(), 0.0);
    k.retarget(&set);
    let got = run(&mut k, &[&x]);
    assert!(
        got[0]
            .iter()
            .zip(&want[0])
            .all(|(a, b)| a.to_bits() == b.to_bits()),
        "a reset bank differs from a fresh one"
    );
    // Straight after the reset, with no `retarget`: the fresh bank splits and sums at the octave
    // splits, and a reset one does the same.
    let mut fresh = bank(1, 3);
    let want = run(&mut fresh, &[&x]);
    let mut k = bank(1, 3);
    k.retarget(&loud);
    run(&mut k, &[&vec![0.9f32; 5_000]]);
    k.reset();
    let got = run(&mut k, &[&x]);
    assert!(
        got[0]
            .iter()
            .zip(&want[0])
            .all(|(a, b)| a.to_bits() == b.to_bits()),
        "a reset bank differs from a fresh one without a `retarget`"
    );
}

/// A threshold control signal that ends inside a chunk drives the frames it has and no others: with
/// instant followers, frames up to the control signal's end are reduced by the curve at the control signal's
/// -30 dB and the frames after it are at the set 0 dB threshold, unreduced, chunk or not.
#[test]
fn a_signal_that_ends_inside_a_chunk_drives_its_own_frames() {
    let mut set = unity(&[200.0, 3_000.0]);
    set.bands[0] = band(0.0, 0.0, 4.0, 0.0, 0.0, 0.0);
    let mut k = bank(1, 3);
    k.retarget(&set);
    run(&mut k, &[&vec![0.5f32; SETTLE]]);
    // 70 frames of control signal over 300 of audio: the end falls inside the second chunk of 64.
    let signal = vec![-30.0f32; 70];
    let x = vec![0.5f32; 300];
    let mut out = vec![0.0f32; 300];
    let mut driven = MultibandSignals::default();
    driven.thresholds[0] = Some(Mono::new(&signal));
    let (ins, mut outs) = ([&x[..]], [&mut out[..]]);
    k.process(
        driven,
        &mut InputOutput::paired(
            Planes::new(&ins).unwrap(),
            PlanesMut::new(&mut outs).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let reduced = 0.5 * linear(-hard(db(0.5), -30.0, 4.0));
    for (i, &y) in out.iter().enumerate().take(70) {
        assert!((f64::from(y) - reduced).abs() < 1e-4, "frame {i}: {y}");
    }
    for (i, &y) in out.iter().enumerate().skip(70) {
        assert!((y - 0.5).abs() < 1e-4, "frame {i} past the signal: {y}");
    }
}

/// The crossover inside the multiband stays within the derived bound when its split moves
/// between 1 Hz and its cap on every grid period (16 frames) under full-scale input, for 10^5
/// frames: every band is within `T^(2 n)` of the input for `n` splits (`T` the tap bound of a
/// section at the largest gain, `tutti_fixtures::tpt`), the unity compressors pass them as
/// they are, and the output is their sum, `n + 1` bands.
#[test]
fn the_crossover_inside_stays_within_the_bound_under_a_switching_split() {
    let tap = tutti_fixtures::tpt::tpt_section_bound((PI * crate::SOLVE_CEILING).tan() * 1.001).tap;
    for (bands, low, high) in [
        (2usize, vec![1.0f64], vec![23_952.0f64]),
        (3, vec![1.0, 2.0], vec![23_951.0, 23_952.0]),
        (4, vec![1.0, 2.0, 3.0], vec![23_950.0, 23_951.0, 23_952.0]),
    ] {
        let bound = bands as f64 * tap.powi(2 * (bands as i32 - 1));
        for alternate in [false, true] {
            let mut k = bank(1, bands);
            k.retarget(&unity(&low));
            let mut peak = 0.0f64;
            for period in 0..6_250usize {
                k.retarget(&unity(if period.is_multiple_of(2) {
                    &low
                } else {
                    &high
                }));
                let x: Vec<f32> = (0..16)
                    .map(|i| if alternate { [1.0, -1.0][i % 2] } else { 1.0 })
                    .collect();
                for y in &run(&mut k, &[&x])[0] {
                    peak = peak.max(f64::from(y.abs()));
                }
            }
            assert!(
                peak.is_finite() && peak <= bound,
                "{bands} bands, alternating {alternate}: {peak} over {bound}"
            );
        }
    }
}

/// A knee past 400 dB is held there, on a band too: a knee of 1e20 or 1e200 renders finite with
/// the band's level inside it, and once the level has moved (a settled DC of 0.5, then of 0.1)
/// the band's reduction is back on the static curve of the 400 dB knee.
#[test]
fn a_huge_knee_is_held_and_a_band_returns_to_the_static_curve() {
    for knee in [1.0e20f64, 1.0e200] {
        let mut set = unity(&[200.0, 3_000.0]);
        set.bands[0] = band(-20.0, knee, 4.0, 0.001, 0.001, 0.0);
        let mut k = bank(1, 3);
        k.retarget(&set);
        let out = run(&mut k, &[&vec![0.5f32; SETTLE]]);
        assert!(out[0].iter().all(|s| s.is_finite()), "knee {knee}");
        let out = run(&mut k, &[&vec![0.0f32; 4_800]]);
        assert!(out[0].iter().all(|s| s.is_finite()), "knee {knee}: silence");
        let out = run(&mut k, &[&vec![0.1f32; SETTLE]]);
        let want = 0.1 * linear(-curve(db(0.1), -20.0, 4.0, 400.0));
        let got = mean(&out[0][SETTLE - 2_000..]);
        assert!(
            (got - want).abs() < 1e-3 * want,
            "knee {knee}: {got} vs {want}"
        );
    }
}

/// An `f64` multiband whose bands all sit under their thresholds is the crossover's sum: the
/// Butterworth all-pass of the split, as the recursion of `(s^2 - sqrt2 s + 1) / (s^2 + sqrt2 s
/// + 1)` in `f64`, frame for frame to 1e-12 on a signal of more than 24 bits.
#[test]
fn an_f64_multiband_under_every_threshold_is_the_crossover_sum() {
    use crate::testing::butterworth_all_pass;
    use tutti_fixtures::signal::{assert_close, wide_signal};
    let n = 4_800;
    let x = wide_signal(n);
    let mut k = MultibandBank::<f64>::new(
        BankShape::new(ChannelTopology::MONO, MultibandBands::of(2)),
        RATE,
    );
    // The signal is near 0.25 (-12 dB), 12 dB under the threshold of every band.
    k.retarget(
        &MultibandSettings::new()
            .crossover(hz(&[1_000.0]))
            .bands([band(0.0, 0.0, 8.0, 0.01, 0.1, 0.0); MAX_BANDS]),
    );
    let mut out = vec![0.0f64; n];
    k.process(
        MultibandSignals::default(),
        &mut InputOutput::paired(
            Planes::new(&[&x[..]]).unwrap(),
            PlanesMut::new(&mut [&mut out[..]]).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_close(&out, &butterworth_all_pass(&x, 1_000.0, RATE.get()), 1e-12);
}

/// A fresh settings value is the do-nothing multiband: the default compressor in every band,
/// neither bypassed nor soloed, the splits at 200 Hz, 1 kHz and 5 kHz.
#[test]
fn new_settings_split_at_200_1000_5000_and_leave_every_band_alone() {
    let set = MultibandSettings::new();
    let f = set.crossover.frequencies;
    assert_eq!(f[..3], [Hz::new(200.0), Hz::new(1_000.0), Hz::new(5_000.0)]);
    assert_eq!(f[3..], [Hz::ZERO; 2]);
    for b in set.bands {
        assert_eq!(b.compressor, CompressorControls::DEFAULT);
        assert!(!b.bypass && !b.solo);
    }
}

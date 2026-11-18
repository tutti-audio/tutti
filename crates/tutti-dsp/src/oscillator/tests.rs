use super::*;
use core::f64::consts::PI;

const RATE: SampleRate = SampleRate::SR_48K;

fn render(wave: OscillatorWave, hz: f64, len: usize) -> Vec<f32> {
    let mut osc = Oscillator::new(wave);
    (0..len).map(|_| osc.step(Hz(hz), RATE)).collect()
}

/// The amplitude of the `hz` component of `x` over a whole number of its
/// periods.
fn amplitude_at(x: &[f32], hz: f64) -> f64 {
    let w = TAU * hz / RATE.get();
    let (mut re, mut im) = (0.0, 0.0);
    for (n, &v) in x.iter().enumerate() {
        re += f64::from(v) * libm::cos(w * n as f64);
        im += f64::from(v) * libm::sin(w * n as f64);
    }
    2.0 * re.hypot(im) / x.len() as f64
}

#[test]
fn the_sine_is_sin_two_pi_f_t() {
    let out = render(OscillatorWave::Sine, 1_234.5, 4_800);
    for (n, &y) in out.iter().enumerate() {
        let exact = libm::sin(TAU * 1_234.5 * n as f64 / 48_000.0);
        assert!((f64::from(y) - exact).abs() < 1e-6, "sample {n}");
    }
}

/// 1 kHz at 48 kHz is 48 samples a cycle, so the naive saw's samples sum
/// to -1 and the edge correction adds the +1 that makes the cycle's mean 0.
#[test]
fn a_saw_and_a_square_have_no_dc_over_whole_periods() {
    for wave in [
        OscillatorWave::Saw,
        OscillatorWave::Square,
        OscillatorWave::Triangle,
    ] {
        let out = render(wave, 1_000.0, 48 * 100);
        let mean = out.iter().map(|&v| f64::from(v)).sum::<f64>() / out.len() as f64;
        assert!(mean.abs() < 1e-6, "{wave:?} mean {mean}");
    }
}

#[test]
fn a_square_holds_plus_and_minus_one_between_its_edges() {
    let out = render(OscillatorWave::Square, 1_000.0, 96);
    // Period 48: +1 over frames 1..24, -1 over 25..48 (the edges at 0 and 24
    // are within a sample of the correction).
    for &y in &out[1..24] {
        assert!((y - 1.0).abs() < 1e-6, "{y}");
    }
    for &y in &out[25..48] {
        assert!((y + 1.0).abs() < 1e-6, "{y}");
    }
    // An edge on a sample is the midpoint of the jump.
    assert!(out[0].abs() < 1e-6 && out[24].abs() < 1e-6);
}

#[test]
fn a_saw_rises_from_minus_one_by_two_a_cycle() {
    let out = render(OscillatorWave::Saw, 1_000.0, 48);
    for (n, &y) in out.iter().enumerate().skip(1) {
        let exact = 2.0 * n as f64 / 48.0 - 1.0;
        assert!((f64::from(y) - exact).abs() < 1e-6, "sample {n}");
    }
}

#[test]
fn a_triangle_is_minus_one_to_one_and_back_away_from_its_corners() {
    let out = render(OscillatorWave::Triangle, 1_000.0, 48);
    for (n, &y) in out.iter().enumerate() {
        let p = n as f64 / 48.0;
        let naive = 1.0 - 4.0 * (p - 0.5).abs();
        // A corner's correction reaches one sample either way, and is at
        // most 8 * dt / 6 there.
        let near_corner = n <= 1 || n >= 47 || (23..=25).contains(&n);
        let bound = if near_corner { 8.0 / 48.0 / 6.0 } else { 1e-6 };
        assert!((f64::from(y) - naive).abs() <= bound + 1e-6, "sample {n}");
    }
}

/// A corner on a sample is smoothed by `8 * dt * (1/6)` of a sample's rise:
/// the peak sits that far under the naive 1, the trough that far over -1.
#[test]
fn a_triangle_corner_on_a_sample_is_rounded_by_the_blamp_figure() {
    let out = render(OscillatorWave::Triangle, 1_000.0, 48);
    let round = 8.0 / 48.0 / 6.0;
    assert!(
        (f64::from(out[24]) - (1.0 - round)).abs() < 1e-6,
        "{}",
        out[24]
    );
    assert!(
        (f64::from(out[0]) - (-1.0 + round)).abs() < 1e-6,
        "{}",
        out[0]
    );
}

/// 5 kHz at 48 kHz is 48 samples a cycle holding 5 cycles, so every
/// spectral line is a multiple of 1 kHz and only 5, 10, 15 and 20 kHz are
/// harmonics of the tone. The naive saw's harmonic `k` has amplitude
/// `2/(pi k)` and folds to `|5k mod 48|` kHz: the 7th, `2/(7 pi)` = 0.091,
/// lands on 13 kHz (`35 - 48`), and 13 kHz is no harmonic of the tone. The
/// band-limited saw holds that line 16 dB under the one harmonic, and every
/// other non-harmonic line under 0.012, except the two within 1 kHz of
/// Nyquist, where the two-sample kernel attenuates least (under 0.05).
#[test]
fn the_band_limited_saw_leaves_the_aliased_lines_quiet() {
    let out = render(OscillatorWave::Saw, 5_000.0, 48 * 100);
    let fundamental = amplitude_at(&out, 5_000.0);
    assert!((fundamental - 2.0 / PI).abs() < 0.05, "{fundamental}");
    let at_13k = amplitude_at(&out, 13_000.0);
    assert!(at_13k < 0.15 * 2.0 / (7.0 * PI), "13 kHz holds {at_13k}");
    let harmonics = [5_000.0, 10_000.0, 15_000.0, 20_000.0];
    for khz in 1..24 {
        let hz = f64::from(khz) * 1_000.0;
        if harmonics.contains(&hz) {
            continue;
        }
        let bound = if hz > 17_000.0 && hz != 19_000.0 {
            0.05
        } else {
            0.012
        };
        let a = amplitude_at(&out, hz);
        assert!(a < bound, "{hz} Hz holds {a}");
    }
}

#[test]
fn a_frequency_that_is_not_finite_holds_the_phase() {
    let mut osc = Oscillator::new(OscillatorWave::Saw);
    osc.step(Hz(1_000.0), RATE);
    let held = osc;
    osc.step(Hz(f64::NAN), RATE);
    assert_eq!(osc, held);
    osc.step(Hz(-5.0), RATE);
    assert_eq!(osc, held);
}

#[test]
fn reset_returns_the_phase_to_zero() {
    let mut osc = Oscillator::new(OscillatorWave::Sine);
    osc.step(Hz(1_000.0), RATE);
    osc.reset();
    assert_eq!(osc, Oscillator::new(OscillatorWave::Sine));
}

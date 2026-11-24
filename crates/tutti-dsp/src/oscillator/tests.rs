use super::*;
use core::f64::consts::{PI, TAU};
use tutti_fixtures::signal::amplitude_at;
use tutti_units::Hz;
use tutti_units::PhaseStep;
use tutti_units::SampleRate;

const RATE: SampleRate = SampleRate::SR_48K;

// The tests other than the `f64` ones render at `F = f32`: the wave is computed in `f32` from
// the phase truncated to `2^-24` cycles, so their tolerances are `f32`'s.

fn render(wave: OscillatorWave, hz: f64, len: usize) -> Vec<f32> {
    let mut osc = Oscillator::new(wave);
    let step = OscillatorStep::new(Hz::new(hz), RATE);
    (0..len).map(|_| osc.step(step)).collect()
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
    // An edge on a sample is the midpoint of the jump. At `dt = 1/48` (1 kHz at 48 kHz) the
    // `f32` phase is the running phase truncated to `2^-24` cycles, so the offset from the edge
    // `x = d / dt` is off by up to `2^-24 * 48` samples, and the jump's correction (slope at
    // most 1, scaled by 2) moves the sample by twice that.
    let edge = 2.0 * 48.0 * 2f32.powi(-24);
    assert!(out[0].abs() <= edge && out[24].abs() <= edge, "{out:?}");
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
    let fundamental = amplitude_at(&out, Hz::new(5_000.0), RATE);
    assert!((fundamental - 2.0 / PI).abs() < 0.05, "{fundamental}");
    let at_13k = amplitude_at(&out, Hz::new(13_000.0), RATE);
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
        let a = amplitude_at(&out, Hz::new(hz), RATE);
        assert!(a < bound, "{hz} Hz holds {a}");
    }
}

#[test]
fn a_frequency_that_is_not_finite_holds_the_phase() {
    let mut osc = Oscillator::new(OscillatorWave::Saw);
    osc.step(OscillatorStep::<f32>::new(Hz::new(1_000.0), RATE));
    let held = osc;
    osc.step(OscillatorStep::<f32>::new(Hz::new(f64::NAN), RATE));
    assert_eq!(osc, held);
    osc.step(OscillatorStep::<f32>::new(Hz::new(-5.0), RATE));
    assert_eq!(osc, held);
}

#[test]
fn reset_returns_the_phase_to_zero() {
    let mut osc = Oscillator::new(OscillatorWave::Sine);
    osc.step(OscillatorStep::<f32>::new(Hz::new(1_000.0), RATE));
    osc.reset();
    assert_eq!(osc, Oscillator::new(OscillatorWave::Sine));
}

/// An `f64` step is the closed-form sine to `f64` precision: a quarter-rate sine is
/// `0, 1, 0, -1` and a step at `1234.5 Hz` is `sin(2π f n / fs)`, so no step narrows to `f32`.
#[test]
fn an_f64_sine_is_the_closed_form_to_f64_precision() {
    let mut osc = Oscillator::new(OscillatorWave::Sine);
    for n in 0..4_800 {
        let exact = libm::sin(TAU * 1_234.5 * f64::from(n) / RATE.get());
        let y: f64 = osc.step(OscillatorStep::new(Hz::new(1_234.5), RATE));
        // The phase is a 64-bit count of cycles, whose rounding of `1234.5 / 48000` is
        // 2^-64 a step: far under the bound.
        assert!((y - exact).abs() < 1e-12, "sample {n}: {y} vs {exact}");
    }
}

/// At `f64` the naive part of each wave is its closed form away from the edges: the phase is
/// the running phase to `2^-53` cycles, so a saw (`2p - 1`), a square (`+-1`) and a triangle
/// (`1 - 4|p - 1/2|`) at 1 kHz and 48 kHz hold to 1e-12 at the frames more than a sample from
/// an edge.
#[test]
fn an_f64_saw_square_and_triangle_are_their_closed_forms_away_from_the_edges() {
    let waves: [(OscillatorWave, fn(f64) -> f64); 3] = [
        (OscillatorWave::Saw, |p| 2.0 * p - 1.0),
        (OscillatorWave::Square, |p| if p < 0.5 { 1.0 } else { -1.0 }),
        (OscillatorWave::Triangle, |p| 1.0 - 4.0 * (p - 0.5).abs()),
    ];
    for (wave, closed) in waves {
        let mut osc = Oscillator::new(wave);
        for n in 0..48 {
            let y: f64 = osc.step(OscillatorStep::new(Hz::new(1_000.0), RATE));
            let near_edge = n <= 1 || n >= 47 || (23..=25).contains(&n);
            if !near_edge {
                let want = closed(f64::from(n) / 48.0);
                assert!(
                    (y - want).abs() < 1e-12,
                    "{wave:?} frame {n}: {y} vs {want}"
                );
            }
        }
    }
}

/// The closed-form saw and square at a phase `count` (of `2^64` cycles) with the step `inc`: the
/// naive wave less (saw) or plus (square) twice the PolyBLEP of the exact distance from the edge
/// in steps, formed from the counts in `f64`.
fn closed_form(wave: OscillatorWave, count: u64, inc: u64) -> f64 {
    let blep = |x: f64| {
        if x < 0.0 {
            0.5 * (x + 1.0) * (x + 1.0)
        } else {
            -0.5 * (1.0 - x) * (1.0 - x)
        }
    };
    let cycles = 2f64.powi(64);
    let near = |edge: u64| {
        let d = i128::from(count.wrapping_sub(edge) as i64);
        (-i128::from(inc) <= d && d < i128::from(inc)).then(|| d as f64 / inc as f64)
    };
    let p = count as f64 / cycles;
    match wave {
        OscillatorWave::Saw => 2.0 * p - 1.0 - near(0).map_or(0.0, |x| 2.0 * blep(x)),
        _ => {
            let naive = if p < 0.5 { 1.0 } else { -1.0 };
            naive + near(0).map_or(0.0, |x| 2.0 * blep(x))
                - near(1 << 63).map_or(0.0, |x| 2.0 * blep(x))
        }
    }
}

/// At 0.01 Hz and 48 kHz (a step of `2.1e-7` cycles, six `f32` ulps of the phase) the edge
/// correction of an `f32` saw and square is the closed form's: the distance from the edge is
/// taken from the exact count, so it is exact whatever the float. The remaining error is the
/// naive wave's, the phase truncated to `2^-24` cycles (`2 * 2^-24` in a saw's `2p - 1`, none in
/// a square's) and the rounding of the product with the correction (`2^-24` each of the two
/// terms, `blep <= 1/2`): `5 * 2^-24`. A distance taken from the `f32` phase would be off by up
/// to `2^-24 / 2.1e-7 = 0.08` steps, a correction error of 0.08.
#[test]
fn an_f32_saw_and_square_at_0_01_hz_hold_the_edge_correction_of_the_closed_form() {
    let step = OscillatorStep::<f32>::new(Hz::new(0.01), RATE);
    let inc = PhaseStep::from(PhaseIncrement::new(0.01 / RATE.get())).get();
    let bound = 5.0 * 2f64.powi(-24);
    for wave in [OscillatorWave::Saw, OscillatorWave::Square] {
        // Start 3.3 steps before the edges at the wrap and at the half.
        for edge in [0u64, 1 << 63] {
            if wave == OscillatorWave::Saw && edge != 0 {
                continue;
            }
            let start = edge.wrapping_sub(inc * 10 / 3);
            let mut osc = Oscillator::new(wave);
            osc.phase = RunningPhase::START.advance(PhaseStep::from_count(start));
            for n in 0..8 {
                let count = start.wrapping_add(inc * n);
                let y: f32 = osc.step(step);
                let want = closed_form(wave, count, inc);
                assert!(
                    (f64::from(y) - want).abs() <= bound,
                    "{wave:?} edge {edge} frame {n}: {y} against {want}"
                );
            }
        }
    }
}

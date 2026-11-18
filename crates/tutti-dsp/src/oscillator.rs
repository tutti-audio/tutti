//! The band-limited oscillator: a phase accumulator read through a sine, or
//! through a saw, square or triangle corrected at its discontinuities.
//!
//! The phase is a [`RunningPhase`], a `u64` count of `2^-64` cycles, so a long render does
//! not drift and a 0.01 Hz rate is exact; `p` below is its top 53 bits in cycles, `[0, 1)`.
//! A sine is `sin(2πp)`, exact. The other three are the naive closed forms (`2p - 1`; `+1` then `-1`; `1 - 4|p - 1/2|`) with a polynomial
//! residual added within one sample of each discontinuity (Välimäki and
//! Huovilainen, "Antialiasing Oscillators in Subtractive Synthesis", 2007):
//! PolyBLEP at a jump of the signal (the saw's wrap, the square's two edges) and
//! PolyBLAMP at a jump of its slope (the triangle's two corners). The residual
//! is the difference between the signal band-limited by a two-sample
//! polynomial kernel and the naive one, so what it removes is the energy the
//! naive form folds back below Nyquist.

use core::f64::consts::TAU;
use tutti_buffers::Float;
use tutti_units::{Hz, PhaseIncrement, PhaseStep, RunningPhase, SampleRate};

use crate::finite::finite_or;

/// The shape an [`Oscillator`] reads its phase through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OscillatorWave {
    /// `sin(2πp)`: starts at 0, rising.
    Sine,
    /// `2p - 1` with PolyBLEP at the wrap: a rising ramp from -1.
    Saw,
    /// `+1` for the first half-cycle and `-1` for the second, with PolyBLEP at
    /// both edges.
    Square,
    /// `1 - 4|p - 1/2|` with PolyBLAMP at both corners: -1 at the cycle start,
    /// +1 at the half.
    Triangle,
}

/// The correction a jump of unit height adds to the naive step `x` samples
/// from the jump (`x` in `[-1, 1)`): PolyBLEP, `(x + 1)^2 / 2` before it and
/// `-(1 - x)^2 / 2` after.
#[inline]
fn blep(x: f64) -> f64 {
    if x < 0.0 {
        let u = x + 1.0;
        0.5 * u * u
    } else {
        let u = 1.0 - x;
        -0.5 * u * u
    }
}

/// The correction a slope change of one sample's rise per sample adds to the
/// naive corner `x` samples from it (`x` in `[-1, 1)`): PolyBLAMP, the integral
/// of [`blep`], `(1 + x)^3 / 6` before the corner and `(1 - x)^3 / 6` after.
#[inline]
fn blamp(x: f64) -> f64 {
    if x < 0.0 {
        let u = 1.0 + x;
        u * u * u / 6.0
    } else {
        let u = 1.0 - x;
        u * u * u / 6.0
    }
}

/// How far `phase` is from a discontinuity at `edge`, in samples of `dt`
/// cycles each, when it is within one sample of it (wrapping the cycle).
#[inline]
fn near(phase: f64, edge: f64, dt: f64) -> Option<f64> {
    let mut d = phase - edge;
    if d >= 0.5 {
        d -= 1.0;
    } else if d < -0.5 {
        d += 1.0;
    }
    let x = d / dt;
    (-1.0..1.0).contains(&x).then_some(x)
}

/// A scalar oscillator: one [`OscillatorWave`] at a frequency that may change
/// every sample.
///
/// The frequency is clamped to `0..=Nyquist` for the step, so a negative one
/// holds the phase and one past Nyquist runs at Nyquist; a frequency that is
/// not finite holds the phase. A saw, square or triangle never emits a value
/// outside `[-1, 1]` by more than the small overshoot of the polynomial
/// correction.
///
/// # Examples
///
/// ```
/// use tutti_dsp::{Oscillator, OscillatorWave};
/// use tutti_units::{Hz, SampleRate};
///
/// let mut osc = Oscillator::new(OscillatorWave::Sine);
/// let rate = SampleRate::SR_48K;
/// assert_eq!(osc.step(Hz(12_000.0), rate), 0.0);
/// // A quarter of a cycle later: the peak.
/// assert!((osc.step(Hz(12_000.0), rate) - 1.0).abs() < 1e-6);
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Oscillator {
    wave: OscillatorWave,
    phase: RunningPhase,
}

impl Oscillator {
    /// An oscillator of `wave` at phase 0.
    #[must_use]
    pub const fn new(wave: OscillatorWave) -> Self {
        Self {
            wave,
            phase: RunningPhase::START,
        }
    }

    /// The wave it reads.
    #[must_use]
    pub const fn wave(&self) -> OscillatorWave {
        self.wave
    }

    /// Returns the phase to 0.
    pub fn reset(&mut self) {
        self.phase = RunningPhase::START;
    }

    /// The sample at the current phase, then the phase advanced by one sample
    /// of `frequency` at `rate`.
    #[inline]
    pub fn step(&mut self, frequency: Hz, rate: SampleRate) -> f32 {
        // A non-finite frequency holds the phase.
        let f = finite_or(frequency.get(), 0.0);
        let dt = (f / rate.get()).clamp(0.0, 0.5);
        let p = f64::from_phase(self.phase);
        let y = match self.wave {
            OscillatorWave::Sine => libm::sin(TAU * p),
            OscillatorWave::Saw => {
                let mut y = 2.0 * p - 1.0;
                if let Some(x) = near(p, 0.0, dt) {
                    y -= 2.0 * blep(x);
                }
                y
            }
            OscillatorWave::Square => {
                let mut y = if p < 0.5 { 1.0 } else { -1.0 };
                if let Some(x) = near(p, 0.0, dt) {
                    y += 2.0 * blep(x);
                }
                if let Some(x) = near(p, 0.5, dt) {
                    y -= 2.0 * blep(x);
                }
                y
            }
            OscillatorWave::Triangle => {
                let mut y = 1.0 - 4.0 * (p - 0.5).abs();
                // The slope per cycle is -4 before the wrap and +4 after it,
                // +4 before the half and -4 after it: a change of +8 at the
                // wrap and -8 at the half, each `8 * dt` of a sample's rise.
                if let Some(x) = near(p, 0.0, dt) {
                    y += 8.0 * dt * blamp(x);
                }
                if let Some(x) = near(p, 0.5, dt) {
                    y -= 8.0 * dt * blamp(x);
                }
                y
            }
        };
        self.phase = self.phase.advance(PhaseStep::from(PhaseIncrement(dt)));
        y as f32
    }
}

#[cfg(test)]
mod tests;

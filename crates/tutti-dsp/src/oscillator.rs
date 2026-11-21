//! The band-limited oscillator: a phase accumulator read through a sine, or
//! through a saw, square or triangle corrected at its discontinuities.
//!
//! The phase is a [`RunningPhase`], a `u64` count of `2^-64` cycles, so a long render does
//! not drift and a 0.01 Hz rate is exact; `p` below is its top bits in cycles, `[0, 1)`, as
//! many as the float the wave is computed in holds, and the wave is computed in that float.
//! A sine is `sin(2πp)`, its argument reduced from the exact count to the nearest quarter
//! cycle. The other three are the naive closed forms (`2p - 1`; `+1` then `-1`;
//! `1 - 4|p - 1/2|`) with a polynomial residual added within one sample of each
//! discontinuity (Välimäki and Huovilainen, "Antialiasing Oscillators in Subtractive
//! Synthesis", 2007):
//! PolyBLEP at a jump of the signal (the saw's wrap, the square's two edges) and
//! PolyBLAMP at a jump of its slope (the triangle's two corners). The residual
//! is the difference between the signal band-limited by a two-sample
//! polynomial kernel and the naive one, so what it removes is the energy the
//! naive form folds back below Nyquist.

use tutti_buffers::Float;
use tutti_units::{Hz, Phase, PhaseIncrement, PhaseStep, RunningPhase, SampleRate};

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
fn blep<F: Float>(x: F) -> F {
    let half = F::lit(0.5);
    if x < F::ZERO {
        let u = x + F::ONE;
        half * u * u
    } else {
        let u = F::ONE - x;
        -half * u * u
    }
}

/// The correction a slope change of one sample's rise per sample adds to the
/// naive corner `x` samples from it (`x` in `[-1, 1)`): PolyBLAMP, the integral
/// of [`blep`], `(1 + x)^3 / 6` before the corner and `(1 - x)^3 / 6` after.
#[inline]
fn blamp<F: Float>(x: F) -> F {
    let u = if x < F::ZERO { F::ONE + x } else { F::ONE - x };
    u * u * u / F::lit(6.0)
}

/// How far `phase` is from the discontinuity at the phase count `edge`, in steps of `step`
/// (`-1..1`), when it is within one step of it. The distance is the wrapping difference of two
/// exact counts and the quotient is formed from counts, so the position is exact whatever
/// the width of `F`, which it crosses into once.
#[inline]
fn near<F: Float>(phase: RunningPhase, edge: u64, step: PhaseStep) -> Option<F> {
    let distance = i128::from(phase.get().wrapping_sub(edge) as i64);
    let step = i128::from(step.get());
    (-step <= distance && distance < step).then(|| F::from_design(distance as f64 / step as f64))
}

/// `sin(2πp)` for the running phase `p`, in `F`: the phase is reduced to the nearest quarter
/// cycle and the offset from it, a signed count, crosses into `F` as an angle in `-π/4..π/4`
/// with full relative precision, so the sine's zeros are exact to the width of `F` at any
/// phase.
#[inline]
pub(crate) fn sine<F: Float>(phase: RunningPhase) -> F {
    const RADIANS_PER_COUNT: f64 = core::f64::consts::TAU / 18_446_744_073_709_551_616.0;
    let count = phase.get();
    let quarter = count.wrapping_add(1 << 61) >> 62;
    let offset = count.wrapping_sub(quarter << 62) as i64;
    let angle = F::from_design(offset as f64 * RADIANS_PER_COUNT);
    match quarter {
        0 => angle.sin(),
        1 => angle.cos(),
        2 => -angle.sin(),
        _ => -angle.cos(),
    }
}

/// The phase count of the half cycle.
const HALF: u64 = 1 << 63;

/// One sample's advance of an [`Oscillator`] at a frequency: the exact step of the running
/// phase and its width in cycles as data of `F`.
///
/// Built once per block for a frequency that holds, or per sample for one that moves; it is
/// the one crossing of the frequency into the oscillator.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OscillatorStep<F: Float> {
    step: PhaseStep,
    cycles: F,
}

impl<F: Float> OscillatorStep<F> {
    /// The step of `frequency` at `rate`. The frequency is clamped to `0..=Nyquist`, so a
    /// negative one holds the phase and one past Nyquist runs at Nyquist; a frequency that is
    /// not finite holds the phase.
    #[inline]
    #[must_use]
    pub fn new(frequency: Hz, rate: SampleRate) -> Self {
        let f = frequency.finite_or(Hz::ZERO).get();
        let cycles = PhaseIncrement::new((f / rate.get()).clamp(0.0, 0.5));
        Self {
            step: PhaseStep::from(cycles),
            cycles: F::from_unit(cycles),
        }
    }
}

/// A scalar oscillator: one [`OscillatorWave`] at a frequency that may change
/// every sample, given as an [`OscillatorStep`].
///
/// A saw, square or triangle never emits a value
/// outside `[-1, 1]` by more than the small overshoot of the polynomial
/// correction.
///
/// # Examples
///
/// ```
/// use tutti_dsp::{Oscillator, OscillatorStep, OscillatorWave};
/// use tutti_units::{Hz, SampleRate};
///
/// let mut osc = Oscillator::new(OscillatorWave::Sine);
/// let rate = SampleRate::SR_48K;
/// let step = OscillatorStep::<f32>::new(Hz::new(12_000.0), rate);
/// assert_eq!(osc.step(step), 0.0);
/// // A quarter of a cycle later: the peak.
/// assert!((osc.step(step) - 1.0).abs() < 1e-6);
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

    /// An oscillator of `wave` at `phase`: a sine at 0.25 is the cosine of the one at 0.
    #[must_use]
    pub fn at(wave: OscillatorWave, phase: Phase) -> Self {
        Self {
            wave,
            phase: RunningPhase::at(phase),
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

    /// The sample at the current phase, in the float `F` it is wanted in, then the phase
    /// advanced by `step`. The wave is computed in `F`: the sine from the exact phase reduced
    /// to a quarter cycle, the others from the phase read as `F` ([`Float::from_phase`]) with
    /// the distance to each edge taken exactly from the running phase.
    #[inline]
    pub fn step<F: Float>(&mut self, step: OscillatorStep<F>) -> F {
        let phase = self.phase;
        let (half, one) = (F::lit(0.5), F::ONE);
        let y = match self.wave {
            OscillatorWave::Sine => sine(phase),
            OscillatorWave::Saw => {
                let mut y = F::lit(2.0) * F::from_phase(phase) - one;
                if let Some(x) = near(phase, 0, step.step) {
                    y -= F::lit(2.0) * blep(x);
                }
                y
            }
            OscillatorWave::Square => {
                let mut y = if phase.get() < HALF { one } else { -one };
                if let Some(x) = near(phase, 0, step.step) {
                    y += F::lit(2.0) * blep(x);
                }
                if let Some(x) = near(phase, HALF, step.step) {
                    y -= F::lit(2.0) * blep(x);
                }
                y
            }
            OscillatorWave::Triangle => {
                let mut y = one - F::lit(4.0) * (F::from_phase(phase) - half).abs();
                // The slope per cycle is -4 before the wrap and +4 after it,
                // +4 before the half and -4 after it: a change of +8 at the
                // wrap and -8 at the half, each `8 * dt` of a sample's rise.
                let rise = F::lit(8.0) * step.cycles;
                if let Some(x) = near(phase, 0, step.step) {
                    y += rise * blamp(x);
                }
                if let Some(x) = near(phase, HALF, step.step) {
                    y -= rise * blamp(x);
                }
                y
            }
        };
        self.phase = phase.advance(step.step);
        y
    }
}

#[cfg(test)]
mod tests;

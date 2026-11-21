//! A low-frequency oscillator: a sine or a triangle read at the control rate, with a delay and
//! a fade-in.
//!
//! An [`Lfo`] is a control signal, not audio, so its waves are the ideal curves and are not
//! band-limited. The phase is a [`RunningPhase`] that adds a step a frame (or a count of
//! steps a chunk) with a wrapping add, so it does not drift however long the LFO runs, and the
//! value after `n` frames is the closed form of the wave at `n` steps whether `n` came as one
//! chunk or as frames of one.
//!
//! # Delay and fade-in
//!
//! For the delay the LFO holds [`LfoWave::START`], the wave's value at the start of a cycle
//! (zero, for both waves). The cycle starts when the delay ends, which may fall between two
//! frames: the phase is then the exact fraction of a frame past it. A fade-in scales the wave
//! by a gain that rises linearly from zero to one over its time, from the end of the delay.

use tutti_units::{
    Bipolar, Hz, PhaseIncrement, PhaseStep, RunningPhase, SampleRate, Samples, Seconds,
};

use crate::design::exact_frames;
use crate::oscillator::sine;

/// The wave of an [`Lfo`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LfoWave {
    /// `sin(2 pi p)`: zero at the start of a cycle, rising.
    Sine,
    /// Zero at the start of a cycle, rising linearly to 1 at a quarter, falling to -1 at three
    /// quarters, and rising to zero at the end (SF2 2.04 section 8.1.2, generators 21 to 24).
    Triangle,
}

impl LfoWave {
    /// The value at the start of a cycle: zero.
    pub const START: Bipolar = Bipolar::ZERO;

    /// The closed form of the wave at `phase`, in `-1..=1`.
    #[must_use]
    pub fn value_at(self, phase: RunningPhase) -> Bipolar {
        const CYCLE: f64 = 18_446_744_073_709_551_616.0; // 2^64
        let value = match self {
            Self::Sine => sine::<f64>(phase),
            Self::Triangle => {
                let p = phase.get() as f64 / CYCLE;
                if p < 0.25 {
                    4.0 * p
                } else if p < 0.75 {
                    4.0 * (0.5 - p)
                } else {
                    4.0 * (p - 1.0)
                }
            }
        };
        Bipolar::clamped(value)
    }
}

/// A low-frequency oscillator at a rate, after a delay, fading in.
///
/// Built on the control side; nothing it holds grows. [`advance`](Self::advance) moves it
/// on by a chunk of frames (or one) and [`value`](Self::value) reads where it is.
#[derive(Clone, Copy, Debug)]
pub struct Lfo {
    wave: LfoWave,
    sample_rate: SampleRate,
    step: PhaseIncrement,
    /// The phase the running count has at the end of the delay, taken away from it.
    zero: RunningPhase,
    phase: RunningPhase,
    delay: f64,
    fade_in: f64,
    elapsed: u64,
}

impl Lfo {
    /// An LFO of `wave` at `rate`, at the start of its first cycle, with no delay and no
    /// fade-in.
    #[must_use]
    pub fn new(wave: LfoWave, rate: Hz, sample_rate: SampleRate) -> Self {
        Self {
            wave,
            sample_rate,
            step: PhaseIncrement::per_sample(rate, sample_rate),
            zero: RunningPhase::START,
            phase: RunningPhase::START,
            delay: 0.0,
            fade_in: 0.0,
            elapsed: 0,
        }
    }

    /// Holds the start value for `delay` before the first cycle begins.
    #[must_use]
    pub fn delay(mut self, delay: Seconds) -> Self {
        self.delay = exact_frames(delay, self.sample_rate);
        let at_end = PhaseStep::from(self.step * self.delay);
        self.zero = RunningPhase::START.advance(at_end);
        self
    }

    /// Fades the wave in over `fade_in`, linearly from the end of the delay.
    #[must_use]
    pub fn fade_in(mut self, fade_in: Seconds) -> Self {
        self.fade_in = exact_frames(fade_in, self.sample_rate);
        self
    }

    /// Runs at `rate` against `sample_rate` from here on; the phase is kept, and the delay
    /// and the fade-in, counted in frames when they were set, keep their frames.
    pub fn retarget(&mut self, rate: Hz, sample_rate: SampleRate) {
        self.sample_rate = sample_rate;
        self.step = PhaseIncrement::per_sample(rate, sample_rate);
    }

    /// Back to the start: the delay again, and the first cycle after it.
    pub fn reset(&mut self) {
        self.phase = RunningPhase::START;
        self.elapsed = 0;
    }

    /// Moves on by `frames`: the same as `frames` calls with one, as the phase adds
    /// `frames` steps in one wrapping product.
    pub fn advance(&mut self, frames: Samples) {
        let count = frames.get() as u64;
        let step = PhaseStep::from(self.step);
        self.phase = self
            .phase
            .advance(PhaseStep::from_count(step.get().wrapping_mul(count)));
        self.elapsed = self.elapsed.saturating_add(count);
    }

    /// The value now: the start value during the delay, then the wave at its phase, scaled by
    /// the fade-in.
    #[must_use]
    pub fn value(&self) -> Bipolar {
        let past = self.elapsed as f64 - self.delay;
        if past < 0.0 {
            return LfoWave::START;
        }
        let behind = PhaseStep::from_count(0_u64.wrapping_sub(self.zero.get()));
        let wave = self.wave.value_at(self.phase.advance(behind)).get();
        let gain = if self.fade_in > 0.0 {
            (past / self.fade_in).min(1.0)
        } else {
            1.0
        };
        Bipolar::clamped(wave * gain)
    }
}

#[cfg(test)]
mod tests;

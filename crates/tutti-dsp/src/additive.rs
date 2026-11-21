//! Additive synthesis: a bank of sine partials, each a ratio of the played frequency at an
//! amplitude, summed.
//!
//! Each partial is an exact [`Oscillator`] sine, so the output holds the partials and nothing
//! else. A partial whose frequency is at or above the Nyquist is dropped, and the bank is
//! band-limited by construction: no partial folds back.

use tutti_buffers::{Float, MonoMut};
use tutti_units::{Amplitude, FrequencyRatio, Hz, SampleRate};

use crate::{Oscillator, OscillatorStep, OscillatorWave};

/// The most partials a bank has.
pub const MAX_PARTIALS: usize = 256;

tutti_units::ranged! {
    /// How many partials a bank has: `1..=256`.
    #[derive(Eq, Hash, PartialOrd, Ord)]
    PartialCount(u16), PartialCountError, "a partial count within 1..=256",
    |raw| raw >= 1 && raw as usize <= MAX_PARTIALS
}

/// One partial: a multiple of the played frequency at an amplitude.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Partial {
    /// The partial's frequency over the bank's.
    pub ratio: FrequencyRatio,
    /// The amplitude of its sine.
    pub amplitude: Amplitude,
}

impl Partial {
    /// A partial at `ratio` of the played frequency, at `amplitude`.
    #[must_use]
    pub const fn new(ratio: FrequencyRatio, amplitude: Amplitude) -> Self {
        Self { ratio, amplitude }
    }
}

/// The partials of an [`AdditiveBank`]: between 1 and [`MAX_PARTIALS`] of them.
#[derive(Clone, Debug, PartialEq)]
pub struct Partials(Box<[Partial]>);

/// The bank's partials, in order; the error is the count that was refused.
///
/// # Errors
///
/// When the slice is empty or longer than [`MAX_PARTIALS`].
impl TryFrom<&[Partial]> for Partials {
    type Error = PartialCountError;

    fn try_from(partials: &[Partial]) -> Result<Self, PartialCountError> {
        PartialCount::try_from(u16::try_from(partials.len()).unwrap_or(u16::MAX))?;
        Ok(Self(partials.into()))
    }
}

impl Partials {
    /// How many partials there are.
    #[must_use]
    pub fn count(&self) -> PartialCount {
        PartialCount::of(self.0.len() as u16)
    }
}

/// The live controls of an [`AdditiveBank`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AdditiveControls {
    /// The frequency each partial's ratio multiplies; one that is not finite plays nothing.
    pub frequency: Hz,
    /// The output's gain.
    pub level: Amplitude,
}

impl AdditiveControls {
    /// 440 Hz at unity level.
    pub const DEFAULT: Self = Self {
        frequency: Hz::new(440.0),
        level: Amplitude::UNITY,
    };
}

/// A bank of sine partials at one rate.
///
/// Built on the control side; nothing it holds grows. A partial that is dropped for being at
/// or above the Nyquist keeps its phase until it is heard again.
#[derive(Clone, Debug)]
pub struct AdditiveBank<F: Float> {
    partials: Partials,
    rate: SampleRate,
    oscillators: Box<[Oscillator]>,
    /// Each partial's phase step and gain; a gain of 0 is a dropped partial.
    steps: Box<[(OscillatorStep<F>, F)]>,
}

impl<F: Float> AdditiveBank<F> {
    /// A bank of `partials` at `rate`, at the default controls, every phase at 0.
    #[must_use]
    pub fn new(partials: Partials, rate: SampleRate) -> Self {
        let n = partials.0.len();
        let mut bank = Self {
            partials,
            rate,
            oscillators: vec![Oscillator::new(OscillatorWave::Sine); n].into(),
            steps: vec![(OscillatorStep::new(Hz::ZERO, rate), F::ZERO); n].into(),
        };
        bank.retarget(&AdditiveControls::DEFAULT);
        bank
    }

    /// Sets the controls the next block renders from: a partial at or above the Nyquist, or
    /// at a frequency that is not finite, gets a gain of 0. No allocation.
    pub fn retarget(&mut self, controls: &AdditiveControls) {
        let nyquist = Hz::new(self.rate.get() / 2.0);
        let level = controls.level.get();
        for (slot, partial) in self.steps.iter_mut().zip(&*self.partials.0) {
            let frequency = controls.frequency * partial.ratio;
            let heard = frequency.get().is_finite() && frequency < nyquist;
            *slot = (
                OscillatorStep::new(frequency, self.rate),
                if heard {
                    F::from_unit(Amplitude::new(partial.amplitude.get() * level))
                } else {
                    F::ZERO
                },
            );
        }
    }

    /// Back to the start: every phase at 0. No allocation.
    pub fn reset(&mut self) {
        for oscillator in &mut self.oscillators {
            oscillator.reset();
        }
    }

    /// Renders `output.frames()` frames. No allocation.
    pub fn process(&mut self, mut output: MonoMut<'_, F>) {
        let out = output.samples_mut();
        out.fill(F::ZERO);
        for (oscillator, &(step, gain)) in self.oscillators.iter_mut().zip(&*self.steps) {
            if gain == F::ZERO {
                continue;
            }
            for y in out.iter_mut() {
                *y += gain * oscillator.step(step);
            }
        }
    }
}

#[cfg(test)]
mod tests;

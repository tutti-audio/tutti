//! [`AllPassFilter`]: Schroeder's delay-line all-pass.
//!
//! Of `D` frames and coefficient `g`, the [`Lattice`](AllPassForm::Lattice) form is
//! `H(z) = (z^-D - g) / (1 - g z^-D)`: `|H| = 1` at every frequency, with the phase of a
//! delay that is `D (1 + g) / (1 - g)` frames long at DC, and the impulse response `-g` at
//! frame 0 and `(1 - g^2) g^(k-1)` at `k D`. [`Freeverb`](AllPassForm::Freeverb)'s form
//! shares the lattice's loop and takes `v[n - D] - x[n]` for its output instead, which is not
//! unit magnitude. A chain of them at one delay with different coefficients is the phase
//! network of the [`FrequencyShifterBank`](crate::FrequencyShifterBank).

use core::num::NonZeroUsize;
use tutti_buffers::Float;
use tutti_units::{Feedback, Samples};

use crate::AllPassForm;
use crate::delay::DelayRing;

/// One all-pass of `D` frames over the float `F`.
///
/// Built on the control side, where it allocates its ring; nothing after that does.
#[derive(Clone, Debug)]
pub struct AllPassFilter<F: Float> {
    form: AllPassForm,
    ring: DelayRing<F>,
    gain: F,
}

impl<F: Float> AllPassFilter<F> {
    /// A silent [`Lattice`](AllPassForm::Lattice) all-pass, its delay `delay`
    /// frames and `gain` the coefficient `g`, below 1 ([`Feedback`]'s own
    /// contract).
    #[must_use]
    pub fn new(delay: NonZeroUsize, gain: Feedback) -> Self {
        Self {
            form: AllPassForm::Lattice,
            ring: DelayRing::new(delay.get()),
            gain: F::from_unit(gain),
        }
    }

    /// The all-pass of `form`.
    #[must_use]
    pub fn form(mut self, form: AllPassForm) -> Self {
        self.form = form;
        self
    }

    /// The delay in frames.
    #[must_use]
    pub fn delay(&self) -> Samples {
        Samples::new(self.ring.len())
    }

    /// Takes the coefficient `gain`; the ring is kept. No allocation.
    pub fn retarget(&mut self, gain: Feedback) {
        self.gain = F::from_unit(gain);
    }

    /// Back to silence. No allocation.
    pub fn reset(&mut self) {
        self.ring.clear();
    }

    /// One frame in, one out.
    #[inline]
    pub fn tick(&mut self, x: F) -> F {
        let old = self.ring.oldest();
        let v = x + self.gain * old;
        self.ring.push(v);
        match self.form {
            AllPassForm::Lattice => old - self.gain * v,
            AllPassForm::Freeverb => old - x,
        }
    }

    /// Runs `block` through the all-pass in place.
    pub fn process(&mut self, block: &mut [F]) {
        for x in block {
            *x = self.tick(*x);
        }
    }
}

#[cfg(test)]
mod tests;

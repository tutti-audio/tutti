//! Coloured noise: [`Noise`], a seeded white, pink or brown source.
//!
//! White is one [`SplitMix64`] word per sample, uniform in `[-1, 1)` on the float's own grid
//! ([`Float::from_fraction_bits`]).
//!
//! Pink (a power spectral density falling 3 dB an octave) is that white through Paul Kellet's
//! refined filter: six one-pole sections (at 0.99886, 0.99332, 0.969, 0.8665, 0.55 and -0.7616)
//! and a delayed direct term. Its power per bin follows the 3 dB an octave slope to within
//! 0.1 dB from 47 Hz to 1.5 kHz at 48 kHz, and bends away above 3 kHz. It is chosen over the
//! Voss-McCartney sum of held random rows because its output is a filter of the white stream,
//! so one word a sample drives it and the slope has no octave steps from rows that update at
//! fixed intervals, and over Kellet's three-pole economy form because that one is within about
//! 0.5 dB of the slope.
//!
//! Brown (falling 6 dB an octave) is white through a leaky integrator,
//! `y[n] = (1 - 2^-9) y[n-1] + 0.02 w[n]`. The leak moves the pole off the unit circle, so the
//! output is bounded (`|y| < 0.02 / 2^-9 = 10.24`, and a standard deviation of about 0.18)
//! where a pure integrator is a random walk that drifts without bound; the pole sits at a
//! corner of `2^-9 · rate / 2π` (15 Hz at 48 kHz), under which the spectrum flattens instead
//! of rising without limit.
//!
//! The pink and brown coefficients are fixed per-sample constants, not rate-aware: the band
//! over which the pink slope holds is given at 48 kHz and scales with the rate, and so does
//! the brown corner.

use tutti_buffers::{Float, MonoMut};
use tutti_rt::SplitMix64;

/// The spectrum a [`Noise`] has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoiseColour {
    /// A flat spectrum: uniform in `[-1, 1)`, a variance of 1/3.
    White,
    /// A power spectral density falling 3 dB an octave.
    Pink,
    /// A power spectral density falling 6 dB an octave above 15 Hz at 48 kHz, at a standard
    /// deviation of about 0.18.
    Brown,
}

/// A seeded noise source of one [`NoiseColour`], in the float `F`.
///
/// One [`SplitMix64`] word a sample, in sample order, so the stream is continuous across blocks
/// and the same seed gives the same samples. The filter state is finite whenever the stream is,
/// which it always is.
#[derive(Clone, Debug)]
pub struct Noise<F: Float> {
    colour: NoiseColour,
    seed: u64,
    rng: SplitMix64,
    /// The pink filter's six poles' states and its delayed white term, or the brown
    /// integrator's in the first.
    state: [F; 7],
}

impl<F: Float> Noise<F> {
    /// A source of `colour` at the start of the stream `seed` names.
    #[must_use]
    pub const fn new(colour: NoiseColour, seed: u64) -> Self {
        Self {
            colour,
            seed,
            rng: SplitMix64::new(seed),
            state: [F::ZERO; 7],
        }
    }

    /// Back to the start of the stream: the generator reseeded and the filters at rest. No
    /// allocation.
    pub fn reset(&mut self) {
        *self = Self::new(self.colour, self.seed);
    }

    /// The next white sample, in `[-1, 1)`.
    #[inline]
    fn white(&mut self) -> F {
        F::lit(2.0) * F::from_fraction_bits(self.rng.next_u64()) - F::ONE
    }

    /// The next sample of the colour.
    #[inline]
    pub fn next_sample(&mut self) -> F {
        let w = self.white();
        match self.colour {
            NoiseColour::White => w,
            NoiseColour::Pink => {
                let b = &mut self.state;
                let sum = b[0] + b[1] + b[2] + b[3] + b[4] + b[5] + b[6] + w * F::lit(0.5362);
                b[0] = F::lit(0.99886) * b[0] + w * F::lit(0.055_517_9);
                b[1] = F::lit(0.99332) * b[1] + w * F::lit(0.075_075_9);
                b[2] = F::lit(0.969) * b[2] + w * F::lit(0.153_852);
                b[3] = F::lit(0.8665) * b[3] + w * F::lit(0.310_485_6);
                b[4] = F::lit(0.55) * b[4] + w * F::lit(0.532_952_2);
                b[5] = F::lit(-0.7616) * b[5] - w * F::lit(0.016_898);
                b[6] = w * F::lit(0.115_926);
                sum * F::lit(0.11)
            }
            // The pole is `1 - 2^-9`, exact in both widths.
            NoiseColour::Brown => {
                self.state[0] = F::lit(0.998_046_875) * self.state[0] + w * F::lit(0.02);
                self.state[0]
            }
        }
    }

    /// Fills `out` with the next samples of the stream. No allocation.
    pub fn process(&mut self, mut out: MonoMut<'_, F>) {
        for o in out.samples_mut() {
            *o = self.next_sample();
        }
    }
}

#[cfg(test)]
mod tests;

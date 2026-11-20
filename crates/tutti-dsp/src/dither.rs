//! Dither noise: the one per-block step of quantizing a float signal.
//!
//! Dither decorrelates quantization error, and it needs no look-ahead, only an
//! RNG that continues across blocks. [`DitherNoise`] is that RNG and the two
//! noise shapes; which signals to dither, and at what depth, is the caller's.

pub(crate) mod shaping;
pub(crate) mod sink;

use tutti_buffers::{Float, InterleavedMut};
use tutti_rt::SplitMix64;
use tutti_units::Amplitude;

/// The probability density of the noise a [`DitherNoise`] adds.
#[derive(Debug, Clone, Copy, PartialEq, Hash, Eq)]
pub enum DitherShape {
    /// One uniform draw, in `[-0.5, 0.5)` LSB: a variance of `lsb² / 12`.
    Rectangular,
    /// Two uniform draws subtracted (TPDF), in `(-1, 1)` LSB, which
    /// decorrelates the noise from the signal in a way a single draw does not: a
    /// variance of `lsb² / 6`.
    Triangular,
}

/// A [`SplitMix64`] noise source that carries its state across blocks.
///
/// The RNG advances once per sample, in the order the samples are handed over,
/// so the sequence is continuous across blocks and, for interleaved audio,
/// across channels within a block. A fresh sequence per block would correlate
/// the noise to the block grid.
#[derive(Debug, Clone)]
pub struct DitherNoise {
    rng: SplitMix64,
}

impl DitherNoise {
    /// A source at the seed every export starts from.
    #[must_use]
    pub const fn new() -> Self {
        const SEED: u64 = 0x1234_5678;
        Self {
            rng: SplitMix64::new(SEED),
        }
    }

    /// Add noise of `shape`, scaled to `lsb` (one least significant bit of the
    /// target depth in the `[-1, 1]` float domain), to every sample of `audio` in place.
    ///
    /// One draw per sample, in frame order and channel order within a frame. A draw is the
    /// 64-bit word's top bits as a uniform value of `F` ([`Float::from_fraction_bits`]), scaled by
    /// the `lsb` crossed once with [`Real::from_unit`](tutti_units::Real::from_unit), so a block
    /// of `f32` has draws on a `2^-24` grid and one of `f64` on a `2^-53` grid.
    pub fn process<F: Float>(
        &mut self,
        shape: DitherShape,
        lsb: Amplitude,
        mut audio: InterleavedMut<'_, F>,
    ) {
        let lsb = F::from_unit(lsb);
        for s in audio.samples_mut() {
            let noise = match shape {
                DitherShape::Rectangular => self.rectangular::<F>(),
                DitherShape::Triangular => self.triangular::<F>(),
            };
            *s += noise * lsb;
        }
    }

    #[inline]
    fn rectangular<F: Float>(&mut self) -> F {
        self.unit::<F>() - F::lit(0.5)
    }

    #[inline]
    fn triangular<F: Float>(&mut self) -> F {
        let r1 = self.unit::<F>();
        let r2 = self.unit::<F>();
        r1 - r2
    }

    /// The next draw, uniform in `[0, 1)`.
    #[inline]
    fn unit<F: Float>(&mut self) -> F {
        F::from_fraction_bits(self.rng.next_u64())
    }
}

impl Default for DitherNoise {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;

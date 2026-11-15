//! Dither noise: the one per-block step of quantizing a float signal.
//!
//! Dither decorrelates quantization error, and it needs no look-ahead, only an
//! RNG that continues across blocks. [`DitherNoise`] is that RNG and the two
//! noise shapes; which signals to dither, and at what depth, is the caller's.

pub(crate) mod sink;

use crate::Xorshift32;
use std::num::NonZeroU32;
use tutti_types::Amplitude;

/// The probability density of the noise a [`DitherNoise`] adds.
#[derive(Debug, Clone, Copy, PartialEq, Hash, Eq)]
pub enum DitherShape {
    /// One uniform draw, in `[-0.5, 0.5]` LSB: a variance of `lsb² / 12`.
    Rectangular,
    /// Two uniform draws subtracted (TPDF), in `[-1, 1]` LSB, which
    /// decorrelates the noise from the signal in a way a single draw does not: a
    /// variance of `lsb² / 6`.
    Triangular,
}

/// A [`Xorshift32`] noise source that carries its state across blocks.
///
/// The RNG advances once per sample, in the order the samples are handed over,
/// so the sequence is continuous across blocks and, for interleaved audio,
/// across channels within a block. A fresh sequence per block would correlate
/// the noise to the block grid.
#[derive(Debug, Clone)]
pub struct DitherNoise {
    rng: Xorshift32,
}

impl DitherNoise {
    /// A source at the seed every export starts from.
    #[must_use]
    pub const fn new() -> Self {
        const SEED: NonZeroU32 = NonZeroU32::new(0x1234_5678).expect("the seed is nonzero");
        Self {
            rng: Xorshift32::new(SEED),
        }
    }

    /// Add noise of `shape`, scaled to `lsb` (one least significant bit of the
    /// target depth in the `[-1, 1]` float domain), to every sample in place.
    pub fn apply(&mut self, shape: DitherShape, lsb: Amplitude, samples: &mut [f32]) {
        let lsb = lsb.0;
        for s in samples.iter_mut() {
            let noise = match shape {
                DitherShape::Rectangular => self.rectangular(),
                DitherShape::Triangular => self.triangular(),
            };
            *s += noise * lsb;
        }
    }

    #[inline]
    fn rectangular(&mut self) -> f32 {
        Xorshift32::to_unit(self.rng.next_u32()) - 0.5
    }

    #[inline]
    fn triangular(&mut self) -> f32 {
        let r1 = Xorshift32::to_unit(self.rng.next_u32());
        let r2 = Xorshift32::to_unit(self.rng.next_u32());
        r1 - r2
    }
}

impl Default for DitherNoise {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LSB: Amplitude = Amplitude(1.0);

    /// The first rectangular draw is the first xorshift32 word from the seed
    /// 0x12345678, 2_274_908_837, scaled to `[-0.5, 0.5]`: `2274908837 / (2^32 - 1) - 0.5`.
    #[test]
    fn the_first_draw_is_the_seeded_word_scaled() {
        let mut s = [0.0f32; 1];
        DitherNoise::new().apply(DitherShape::Rectangular, LSB, &mut s);
        let want = 2_274_908_837.0_f64 / 4_294_967_295.0 - 0.5;
        assert!((f64::from(s[0]) - want).abs() < 1e-7, "{} vs {want}", s[0]);
    }

    /// The first triangular sample is `(r1 - r2) * lsb` with `r1`, `r2` the
    /// first two seeded words, 2274908837 and 358294691, over `2^32 - 1`.
    #[test]
    fn the_first_triangular_draw_is_the_difference_of_two_seeded_words() {
        let mut s = [0.0f32; 1];
        DitherNoise::new().apply(DitherShape::Triangular, Amplitude(0.25), &mut s);
        let want = (2_274_908_837.0_f64 - 358_294_691.0) / 4_294_967_295.0 * 0.25;
        assert!((f64::from(s[0]) - want).abs() < 1e-7, "{} vs {want}", s[0]);
    }

    #[test]
    fn rectangular_stays_within_half_an_lsb() {
        let mut n = DitherNoise::new();
        let mut s = [0.0f32; 4096];
        n.apply(DitherShape::Rectangular, LSB, &mut s);
        assert!(s.iter().all(|x| x.abs() <= 0.5));
        assert!(s.iter().any(|x| x.abs() > 0.4), "the range is used");
    }

    #[test]
    fn triangular_stays_within_one_lsb() {
        let mut n = DitherNoise::new();
        let mut s = [0.0f32; 4096];
        n.apply(DitherShape::Triangular, LSB, &mut s);
        assert!(s.iter().all(|x| x.abs() <= 1.0));
        assert!(s.iter().any(|x| x.abs() > 0.8), "the range is used");
    }

    /// One block of N equals two of N/2.
    #[test]
    fn the_sequence_continues_across_calls() {
        let mut whole = [0.0f32; 16];
        DitherNoise::new().apply(DitherShape::Triangular, LSB, &mut whole);
        let mut split = [0.0f32; 16];
        let mut n = DitherNoise::new();
        let (a, b) = split.split_at_mut(6);
        n.apply(DitherShape::Triangular, LSB, a);
        n.apply(DitherShape::Triangular, LSB, b);
        assert_eq!(whole, split);
    }
}

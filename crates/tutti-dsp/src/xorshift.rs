//! Marsaglia's xorshift32 generator.

use std::num::NonZeroU32;

/// A xorshift32 pseudo-random generator (Marsaglia 2003, shifts 13, 17, 5): a
/// period of 2^32 - 1 over the nonzero words.
///
/// The state is nonzero because zero is the generator's fixed point. A source
/// that runs eight lanes at once applies the same recurrence to each lane.
///
/// Not cryptographic, and not a stream to compare across seeds: it is a
/// reproducible noise source for dither and phase scatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Xorshift32 {
    state: NonZeroU32,
}

impl Xorshift32 {
    /// A generator whose first word follows `seed`.
    #[must_use]
    pub const fn new(seed: NonZeroU32) -> Self {
        Self { state: seed }
    }

    /// The next word, uniform over `1..=u32::MAX`.
    ///
    /// # Panics
    /// Never: each shift-xor step is a bijection on 32-bit words that fixes
    /// only zero, so a nonzero word stays nonzero.
    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.state.get();
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = NonZeroU32::new(x).expect("xorshift maps nonzero words to nonzero words");
        x
    }

    /// A word as a uniform draw in `[0, 1]`: `word / u32::MAX`, in `f32`.
    #[inline]
    #[must_use]
    pub fn to_unit(word: u32) -> f32 {
        word as f32 / u32::MAX as f32
    }

    /// A word as a uniform draw in `[-1, 1]`: `2 * to_unit(word) - 1`.
    #[inline]
    #[must_use]
    pub fn to_bipolar(word: u32) -> f32 {
        2.0 * Self::to_unit(word) - 1.0
    }
}

#[cfg(test)]
mod tests;

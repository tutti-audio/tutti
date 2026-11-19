//! [`SplitMix64`], the one pseudo-random generator: a stream of 64-bit words
//! from a seed, and the stateless mix that makes each of them.

/// The SplitMix64 generator (Steele, Lea and Flood 2014; Vigna's reference):
/// a 64-bit state advanced by the golden-ratio increment, each output the
/// [`mix`](Self::mix) of the state before the step.
///
/// Fast, allocation-free and the same on every platform, so a seed reproduces
/// its stream anywhere. Well spread, not secret: nothing cryptographic reads it.
/// A seed from the operating system is `getrandom::fill` into eight bytes
/// at the caller, whose own error says what failing to get one means there.
/// A reader that must jump to the `n`-th value without walking there (a random
/// step at a seek) uses [`mix`](Self::mix) on a counter instead of a stream.
///
/// ```
/// use tutti_rt::SplitMix64;
///
/// let mut a = SplitMix64::new(7);
/// let mut b = SplitMix64::new(7);
/// assert_eq!(a.next_u64(), b.next_u64());
/// assert_eq!(SplitMix64::new(0).next_u64(), SplitMix64::mix(0));
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// The golden-ratio increment the state advances by.
    const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

    /// A stream from `seed`.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The next 64-bit word.
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let out = Self::mix(self.state);
        self.state = self.state.wrapping_add(Self::GAMMA);
        out
    }

    /// The next 32-bit word: the high half of the next 64-bit one.
    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// The generator's output for the state `z`: `z` advanced by the
    /// increment, then the two multiply-xorshift rounds. A stateless hash of a
    /// counter or a key, the same one every stream step applies.
    #[inline]
    #[must_use]
    pub const fn mix(mut z: u64) -> u64 {
        z = z.wrapping_add(Self::GAMMA);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[cfg(test)]
mod tests;

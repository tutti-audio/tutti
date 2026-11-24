//! A seeded noise generator and an FNV-1a hash over bits.
//!
//! [`noise`] is a deterministic broadband stimulus for a test; [`Fnv`] folds
//! bytes or the bits of numbers into a 64-bit figure, e.g. to tell two byte
//! streams apart. Both are plain `f32`, `u32` and `u64` code (noise at any `Float` width), so a
//! crate at any layer can dev-depend on this one for them.

use tutti_buffers::Float;
use tutti_rt::SplitMix64;

/// Deterministic broadband noise: an endless stream of `f32` in `[-1, 1)` drawn
/// from [`SplitMix64`], each draw the top 24 bits of its 32-bit output.
///
/// [`Noise::seeded`] feeds the seed to the generator as it is; the generator
/// already decorrelates neighbouring seeds.
#[derive(Clone, Debug)]
pub struct Noise(SplitMix64);

impl Noise {
    /// The stream of `seed`.
    pub fn seeded(seed: u32) -> Self {
        Self(SplitMix64::new(u64::from(seed)))
    }
}

impl Iterator for Noise {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        Some((self.0.next_u32() >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0)
    }
}

/// `len` samples of [`Noise::seeded`]`(seed)`, in `[-1, 1)`.
///
/// Scale it with a `map` (`0.5 * x`) for a stimulus at a level. Each draw is exact
/// in `f32` and widens exactly to `f64`, so every width reads the same stream.
pub fn noise<F: Float>(seed: u32, len: usize) -> Vec<F> {
    Noise::seeded(seed)
        .take(len)
        .map(|x| F::from_f64(f64::from(x)))
        .collect()
}

/// A 64-bit FNV-1a hash, fed bytes or the little-endian bits of a number.
///
/// Feeding is order-sensitive and chunk-insensitive: `f32s(&[a, b])` is
/// `f32(a)` then `f32(b)`, and `bytes` of a concatenation is `bytes` of each
/// part in turn.
///
/// ```
/// use tutti_fixtures::Fnv;
///
/// let mut h = Fnv::new();
/// h.f32s(&[1.0, -0.5]);
/// let mut g = Fnv::new();
/// g.f32(1.0);
/// g.f32(-0.5);
/// assert_eq!(h.finish(), g.finish());
/// assert_eq!(Fnv::new().finish(), 0xcbf2_9ce4_8422_2325);
/// ```
#[derive(Clone, Debug)]
pub struct Fnv(u64);

impl Fnv {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    /// The hash of no bytes.
    pub fn new() -> Self {
        Self(Self::OFFSET)
    }

    /// Feeds `bytes`.
    pub fn bytes(&mut self, bytes: impl IntoIterator<Item = u8>) {
        for byte in bytes {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    /// Feeds `x`, little-endian.
    pub fn u64(&mut self, x: u64) {
        self.bytes(x.to_le_bytes());
    }

    /// Feeds the bits of `x`, little-endian.
    pub fn f32(&mut self, x: f32) {
        self.bytes(x.to_le_bytes());
    }

    /// Feeds the bits of each sample of `samples`, in order.
    pub fn f32s(&mut self, samples: &[f32]) {
        for &x in samples {
            self.f32(x);
        }
    }

    /// Feeds the bits of `x`, little-endian.
    pub fn f64(&mut self, x: f64) {
        self.bytes(x.to_le_bytes());
    }

    /// The hash of everything fed so far.
    pub fn finish(&self) -> u64 {
        self.0
    }
}

impl Default for Fnv {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FNV-1a's published vectors (the empty string, "a", "foobar").
    #[test]
    fn fnv_matches_the_published_vectors() {
        let hash = |s: &str| {
            let mut h = Fnv::new();
            h.bytes(s.bytes());
            h.finish()
        };
        assert_eq!(hash(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(hash("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(hash("foobar"), 0x8594_4171_f739_67e8);
    }

    /// A float is fed as its little-endian bits.
    #[test]
    fn a_float_is_fed_as_its_little_endian_bits() {
        let mut h = Fnv::new();
        h.f32(1.0);
        let mut g = Fnv::new();
        g.bytes([0x00, 0x00, 0x80, 0x3f]);
        assert_eq!(h.finish(), g.finish());
        let mut h = Fnv::new();
        h.f64(1.0);
        let mut g = Fnv::new();
        g.bytes(1.0f64.to_bits().to_le_bytes());
        assert_eq!(h.finish(), g.finish());
    }

    /// The stream is SplitMix64's high 32 bits, top 24 of them `k` as `k / 2^23 - 1`:
    /// seed 0's first output is the published `0xE220A8397B1DCDAF`, so `k` is
    /// `0xE220A8`, exact in `f32`. Every draw stays in `[-1, 1)`.
    #[test]
    fn noise_is_splitmix64_top_24_bits() {
        let x: Vec<f32> = noise(0, 4096);
        assert!(x.iter().all(|s| (-1.0..1.0).contains(s)));
        assert_eq!(x[0], 0xE2_20A8_u32 as f32 / 8_388_608.0 - 1.0);
        assert_eq!(
            noise::<f32>(0, 8),
            x[..8],
            "a longer draw extends a shorter one"
        );
        assert_ne!(noise::<f32>(1, 8), x[..8], "neighbouring seeds differ");
    }
}

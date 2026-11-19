//! The seeded draws the unit tests share.

use tutti_buffers::Float;
use tutti_rt::SplitMix64;

/// The next value of `rng`, uniform over `[lo, hi)` on the `2^-MANTISSA_DIGITS` grid.
pub(crate) fn uniform<F: Float>(rng: &mut SplitMix64, lo: F, hi: F) -> F {
    lo + (hi - lo) * F::from_fraction_bits(rng.next_u64())
}

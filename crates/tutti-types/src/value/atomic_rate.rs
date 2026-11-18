//! [`AtomicSampleRate`] and [`ParkedSampleRate`]: a [`SampleRate`] shared with
//! another thread as one atomic word.

use core::sync::atomic::{AtomicU64, Ordering};
use tutti_units::SampleRate;

/// A [`SampleRate`] another thread reads: one atomic word, the rate's bits.
///
/// Only a `SampleRate` is stored, so a load answers a `SampleRate` with no
/// check and no fallback: the cell cannot hold a non-rate. `Relaxed` suffices
/// where the rate only sizes something; the caller names the ordering it needs
/// otherwise.
#[derive(Debug)]
pub struct AtomicSampleRate(AtomicU64);

impl AtomicSampleRate {
    /// A cell holding `rate`.
    #[must_use]
    pub const fn new(rate: SampleRate) -> Self {
        Self(AtomicU64::new(rate.get().to_bits()))
    }

    /// The rate in the cell.
    #[inline]
    #[must_use]
    pub fn load(&self, order: Ordering) -> SampleRate {
        rate_of(self.0.load(order))
    }

    /// Replaces the rate in the cell.
    #[inline]
    pub fn store(&self, rate: SampleRate, order: Ordering) {
        self.0.store(rate.get().to_bits(), order);
    }

    /// Replaces the rate in the cell and answers the one it held.
    #[inline]
    pub fn swap(&self, rate: SampleRate, order: Ordering) -> SampleRate {
        rate_of(self.0.swap(rate.get().to_bits(), order))
    }
}

/// The rate a cell word holds. Only `new`, `store` and `swap` write the word,
/// each from a `SampleRate`, so it is always a rate; the fallback is never read.
#[inline]
fn rate_of(bits: u64) -> SampleRate {
    SampleRate::new(f64::from_bits(bits)).unwrap_or(SampleRate::SR_48K)
}

/// A [`SampleRate`] one thread leaves for another to take: an
/// [`AtomicSampleRate`] that can also be empty.
///
/// Empty is the bit pattern of `0.0`, which no `SampleRate` has, so a parked
/// rate is never read as empty and the cell is one lock-free word. Parking over
/// a rate nobody took replaces it: the taker only needs the latest.
#[derive(Default)]
pub struct ParkedSampleRate(AtomicU64);

impl core::fmt::Debug for ParkedSampleRate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let bits = self.0.load(Ordering::Relaxed);
        let parked = (bits != 0).then(|| rate_of(bits));
        f.debug_tuple("ParkedSampleRate").field(&parked).finish()
    }
}

impl ParkedSampleRate {
    /// A cell with nothing parked.
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// Parks `rate`, replacing one that was not taken.
    #[inline]
    pub fn park(&self, rate: SampleRate, order: Ordering) {
        self.0.store(rate.get().to_bits(), order);
    }

    /// Takes the parked rate, leaving the cell empty; `None` when nothing is
    /// parked.
    #[inline]
    pub fn take(&self, order: Ordering) -> Option<SampleRate> {
        match self.0.swap(0, order) {
            0 => None,
            bits => Some(rate_of(bits)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::Ordering::Relaxed;

    #[test]
    fn a_parked_rate_is_taken_once_and_the_latest_wins() {
        let cell = ParkedSampleRate::new();
        assert_eq!(cell.take(Relaxed), None, "a new cell is empty");
        cell.park(SampleRate::SR_44K1, Relaxed);
        cell.park(SampleRate::SR_48K, Relaxed);
        assert_eq!(cell.take(Relaxed), Some(SampleRate::SR_48K));
        assert_eq!(cell.take(Relaxed), None, "a take empties the cell");
    }

    #[test]
    fn an_atomic_rate_loads_what_was_stored_and_swap_answers_the_old_one() {
        let cell = AtomicSampleRate::new(SampleRate::SR_44K1);
        assert_eq!(cell.load(Relaxed), SampleRate::SR_44K1);
        cell.store(SampleRate::SR_96K, Relaxed);
        assert_eq!(cell.swap(SampleRate::SR_48K, Relaxed), SampleRate::SR_96K);
        assert_eq!(cell.load(Relaxed), SampleRate::SR_48K);
    }
}

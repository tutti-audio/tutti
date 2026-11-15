//! [`Mint`], the one counter that hands out unique numbers.

use core::num::NonZeroU64;
use core::sync::atomic::{AtomicU64, Ordering};

/// A counter that hands out unique, non-zero numbers in sequence, from 1.
///
/// Lock-free and callable from any thread through `&self`, so a `static`
/// is a process-wide source of ids or of file-name suffixes. A number is
/// never handed out twice by one `Mint`; two `Mint`s are independent.
///
/// The order is the order of the calls that win the counter, so a number says
/// nothing about when its holder was used. A counter that wraps (an epoch)
/// is not a `Mint`: it reuses numbers on purpose.
///
/// ```
/// use tutti_types::Mint;
///
/// static NEXT: Mint = Mint::new();
///
/// assert_eq!(NEXT.mint().get(), 1);
/// assert_eq!(NEXT.mint().get(), 2);
/// ```
#[derive(Debug)]
pub struct Mint(AtomicU64);

impl Mint {
    /// A counter whose first number is 1.
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicU64::new(1))
    }

    /// The next number: one more than the last this counter returned.
    ///
    /// # Panics
    ///
    /// After 2^64 - 1 calls, when the counter would repeat.
    #[must_use]
    pub fn mint(&self) -> NonZeroU64 {
        // Uniqueness is the only property read from the counter, so the
        // increment orders nothing else.
        let next = self.0.fetch_add(1, Ordering::Relaxed);
        NonZeroU64::new(next).expect("a 64-bit counter is not exhausted")
    }
}

impl Default for Mint {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    #[test]
    fn the_first_number_is_one_and_each_is_the_next() {
        let mint = Mint::new();
        let got: Vec<u64> = (0..4).map(|_| mint.mint().get()).collect();
        assert_eq!(got, [1, 2, 3, 4]);
    }

    #[test]
    fn two_counters_are_independent() {
        let (a, b) = (Mint::new(), Mint::new());
        assert_eq!(a.mint().get(), 1);
        assert_eq!(a.mint().get(), 2);
        assert_eq!(b.mint().get(), 1);
    }

    #[test]
    fn threads_never_share_a_number() {
        const THREADS: usize = 8;
        const EACH: usize = 1000;
        let mint = Arc::new(Mint::new());
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let mint = Arc::clone(&mint);
                std::thread::spawn(move || (0..EACH).map(|_| mint.mint().get()).collect::<Vec<_>>())
            })
            .collect();
        let all: Vec<u64> = handles
            .into_iter()
            .flat_map(|h| h.join().expect("a minting thread does not panic"))
            .collect();
        let unique: BTreeSet<u64> = all.iter().copied().collect();
        assert_eq!(
            unique.len(),
            THREADS * EACH,
            "a number was handed out twice"
        );
        assert_eq!(unique.first(), Some(&1));
        assert_eq!(
            unique.last(),
            Some(&((THREADS * EACH) as u64)),
            "a number was skipped"
        );
    }
}
